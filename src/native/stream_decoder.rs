//! Push-based incremental decoder for ClickHouse Native format.
//!
//! Unlike `decode_next_block` which pulls bytes from a `Read` source,
//! `StreamDecoder` receives bytes via `feed()` and emits complete blocks
//! as soon as enough data has accumulated. This eliminates the need for
//! pipes, blocking I/O, or background threads in async contexts.
//!
//! Usage:
//! ```ignore
//! let mut decoder = StreamDecoder::new(DecodeOptions::default());
//! for chunk in byte_chunks {
//!     let blocks = decoder.feed(chunk)?;
//!     for block in blocks { /* process */ }
//! }
//! let final_blocks = decoder.finish()?;
//! ```

use std::io;

use crate::batch::ColBatch;
use crate::native::decode::{
    block_end_binary_types_resume, block_end_resume, decode_scanned_block, DecodeError,
    DecodeOptions, ScanProgress, DBMS_TCP_PROTOCOL_VERSION,
};
use crate::native::varint::ByteReader;
use crate::schema::Schema;

/// Push-based incremental decoder for ClickHouse Native blocks.
///
/// Accumulates bytes via `feed()` and attempts to decode complete blocks
/// after each feed. Partial blocks are retained in the internal buffer
/// until enough data arrives.
///
/// Every block of a query result shares one schema. The decoder retains the
/// first block's schema and rejects any later block whose column names or
/// types differ with [`DecodeError::BlockSchemaMismatch`], matching
/// `decode_all_bytes`.
pub struct StreamDecoder {
    buffer: Vec<u8>,
    /// Byte offset of unconsumed data in `buffer`.
    pos: usize,
    /// Buffer length at which the last completeness scan found the next block
    /// still incomplete. While the buffer has not grown past this, no block can
    /// have completed, so `drain_blocks` skips the re-scan entirely. This elides
    /// redundant scans on feeds that add no bytes past the high-water mark, such
    /// as empty feeds and the `finish()` re-drain. Held in current buffer
    /// coordinates: compaction lowers it by the number of bytes drained.
    scanned: usize,
    /// Verified scan progress into the current partial block, so a re-scan
    /// resumes where the last one stopped instead of restarting from the block
    /// start. Its offsets are relative to `pos`, and compaction drains exactly
    /// `..pos`, so they stay valid without adjustment.
    scan: ScanProgress,
    options: DecodeOptions,
    binary_types: bool,
    finished: bool,
    /// First block's schema; later blocks must match it.
    schema: Option<Schema>,
    /// Blocks decoded so far, for mismatch reporting.
    blocks_seen: usize,
}

impl StreamDecoder {
    pub fn new(options: DecodeOptions) -> Self {
        Self {
            buffer: Vec::new(),
            pos: 0,
            scanned: 0,
            scan: ScanProgress::default(),
            options,
            binary_types: false,
            finished: false,
            schema: None,
            blocks_seen: 0,
        }
    }

    /// Construct a streaming decoder for Native data whose type headers and
    /// Dynamic runtime tables use binary data-type descriptors.
    pub fn new_binary_types(options: DecodeOptions) -> Self {
        Self {
            binary_types: true,
            ..Self::new(options)
        }
    }

    /// Push a chunk of bytes and return any complete blocks that can be
    /// decoded from the accumulated buffer.
    ///
    /// Returns an empty vec if the chunk doesn't complete any block.
    /// Partial data is retained for the next `feed()` or `finish()` call.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<ColBatch>, DecodeError> {
        if self.finished {
            return Err(DecodeError::Io(io::Error::other(
                "feed() called after finish()",
            )));
        }

        self.buffer.extend_from_slice(chunk);
        self.drain_blocks()
    }

    /// Signal that no more bytes will be fed. Returns any remaining
    /// complete blocks and validates that no partial block data remains.
    pub fn finish(&mut self) -> Result<Vec<ColBatch>, DecodeError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;

        // A truly empty stream never enters the completeness scan, so enforce
        // the revision ceiling here too. Nonempty streams validate in their
        // first scan, and completed streams have `blocks_seen > 0`, avoiding a
        // second comparison on either normal path.
        if self.blocks_seen == 0
            && self.buffer.is_empty()
            && self.options.protocol_revision > DBMS_TCP_PROTOCOL_VERSION
        {
            return Err(DecodeError::UnsupportedProtocolRevision {
                revision: self.options.protocol_revision,
                max_supported: DBMS_TCP_PROTOCOL_VERSION,
            });
        }

        let blocks = self.drain_blocks()?;

        // After finishing, any remaining bytes are either empty or a
        // truncated block. Empty is fine (normal EOF). Truncated is an error.
        let remaining = self.buffer.len() - self.pos;
        if remaining > 0 {
            return Err(DecodeError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("stream ended with {remaining} unconsumed bytes (truncated block)"),
            )));
        }

        Ok(blocks)
    }

    /// Number of unconsumed bytes currently buffered, that is, the bytes of a
    /// partial block waiting for more data. Zero when the stream sits exactly
    /// on a block boundary.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len() - self.pos
    }

    /// Try to decode as many complete blocks as possible from the buffer.
    ///
    /// Each iteration first runs the allocation-free [`block_end_resume`]
    /// completeness scan over the unconsumed bytes. Only when it confirms a
    /// whole block is buffered do we run the allocating [`decode_next_block`].
    /// A block that arrives over several feeds therefore allocates its column
    /// buffers exactly once, when the last byte lands, instead of allocating
    /// and discarding them on every partial feed. The scan checkpoints its
    /// progress in `self.scan`, so a block fed in many chunks is walked once
    /// overall rather than re-walked from its start on every feed.
    fn drain_blocks(&mut self) -> Result<Vec<ColBatch>, DecodeError> {
        let mut blocks = Vec::new();

        loop {
            let data = &self.buffer[self.pos..];
            if data.is_empty() {
                break;
            }

            // High-water mark: the previous scan found an incomplete block and
            // consumed all `self.scanned` available bytes reaching for its end.
            // If the buffer has not grown past that, no block can have completed,
            // so skip the redundant re-scan from `self.pos`.
            if self.buffer.len() <= self.scanned {
                break;
            }

            // The scan keeps its checkpoint only across "need more bytes"; any
            // other outcome clears `self.scan`, so a checkpoint never carries
            // into the next block or past an error.
            let scanned_end = if self.binary_types {
                block_end_binary_types_resume(data, &self.options, &mut self.scan)
            } else {
                block_end_resume(data, &self.options, &mut self.scan)
            };
            match scanned_end {
                Ok(Some(end)) => {
                    // A full block is buffered. The allocating decode now reads
                    // exactly `data[..end]`; completeness was just verified with
                    // the same framing, so it cannot hit EOF.
                    let mut reader = ByteReader::new(&data[..end]);
                    let decoded =
                        decode_scanned_block(&mut reader, &self.options, self.binary_types)?;
                    match decoded {
                        Some(batch) => {
                            match &self.schema {
                                None => self.schema = Some(batch.schema.clone()),
                                Some(first) => {
                                    if batch.schema != *first {
                                        return Err(DecodeError::BlockSchemaMismatch {
                                            block_index: self.blocks_seen,
                                        });
                                    }
                                }
                            }
                            self.blocks_seen += 1;
                            blocks.push(batch);
                        }
                        // `block_end` returned `Some`, so a block is present.
                        None => unreachable!("block_end confirmed a complete block"),
                    }
                    self.pos += end;
                    self.scanned = 0;
                    // Try to decode another block from the remaining bytes.
                }
                Ok(None) => {
                    // Clean end-of-stream at a block boundary: no more blocks.
                    self.pos = self.buffer.len();
                    break;
                }
                Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    // Block started but not fully buffered. Remember how far the
                    // buffer reached so the next feed only re-scans once it has
                    // grown, then wait for more data.
                    self.scanned = self.buffer.len();
                    break;
                }
                Err(e) => {
                    // Real decode error (unsupported type, corrupt data, etc.)
                    return Err(e);
                }
            }
        }

        // Compact buffer: remove consumed bytes to prevent unbounded growth.
        if self.pos > 0 {
            self.buffer.drain(..self.pos);
            self.scanned = self.scanned.saturating_sub(self.pos);
            self.pos = 0;
        }

        Ok(blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::Column;
    use crate::native::decode::DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION;
    use crate::native::protocol::LC_HAS_ADDITIONAL_KEYS_BIT;
    use crate::native::varint::write_varint;

    /// Helper: build a Native format block with one Int64 column.
    fn make_int64_block(name: &str, values: &[i64]) -> Vec<u8> {
        let mut buf = Vec::new();
        // num_cols = 1
        write_varint(&mut buf, 1);
        // num_rows
        write_varint(&mut buf, values.len() as u64);
        // column header
        let name_bytes = name.as_bytes();
        write_varint(&mut buf, name_bytes.len() as u64);
        buf.extend_from_slice(name_bytes);
        let type_name = b"Int64";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        // column data
        for &v in values {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// Helper: build a Native format block with one String column (no framing).
    fn make_string_block(name: &str, values: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1); // num_cols
        write_varint(&mut buf, values.len() as u64); // num_rows
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"String";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        for &s in values {
            write_varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        buf
    }

    /// Helper: build one nullable UInt8 sum-state column (no framing).
    fn make_nullable_sum_block(name: &str, num_rows: usize, states: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, num_rows as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"AggregateFunction(sum, Nullable(UInt8))";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        buf.extend_from_slice(states);
        buf
    }

    /// Helper: build one canonical nothingNull state column (no framing).
    fn make_nothing_null_block(name: &str, num_rows: usize, states: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, num_rows as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"AggregateFunction(nothingNull, Nullable(Nothing))";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        buf.extend_from_slice(states);
        buf
    }

    /// Helper: build one Nullable(String) column block (no framing).
    fn make_nullable_string_block(name: &str, values: &[Option<&str>]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, values.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"Nullable(String)";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        for v in values {
            buf.push(if v.is_none() { 0x01 } else { 0x00 });
        }
        for v in values {
            let s = v.unwrap_or("");
            write_varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        buf
    }

    /// Helper: build one LowCardinality(String) column block with u8 indexes
    /// (no framing).
    fn make_lc_string_block(name: &str, dictionary: &[&str], indices: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, indices.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"LowCardinality(String)";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        buf.extend_from_slice(&1u64.to_le_bytes()); // key version
        buf.extend_from_slice(&LC_HAS_ADDITIONAL_KEYS_BIT.to_le_bytes()); // width tag 0 = u8
        buf.extend_from_slice(&(dictionary.len() as u64).to_le_bytes());
        for s in dictionary {
            write_varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        buf.extend_from_slice(&(indices.len() as u64).to_le_bytes());
        buf.extend_from_slice(indices);
        buf
    }

    /// Helper: build one Array(String) column block (no framing).
    fn make_array_string_block(name: &str, rows: &[&[&str]]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, rows.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"Array(String)";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        let mut end = 0u64;
        for row in rows {
            end += row.len() as u64;
            buf.extend_from_slice(&end.to_le_bytes());
        }
        for row in rows {
            for s in *row {
                write_varint(&mut buf, s.len() as u64);
                buf.extend_from_slice(s.as_bytes());
            }
        }
        buf
    }

    /// Helper: build one two-column block, Int64 then String (no framing).
    fn make_mixed_block(ids: &[i64], names: &[&str]) -> Vec<u8> {
        assert_eq!(ids.len(), names.len());
        let mut buf = Vec::new();
        write_varint(&mut buf, 2);
        write_varint(&mut buf, ids.len() as u64);
        write_varint(&mut buf, 2);
        buf.extend_from_slice(b"id");
        write_varint(&mut buf, 5);
        buf.extend_from_slice(b"Int64");
        for &v in ids {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        write_varint(&mut buf, 4);
        buf.extend_from_slice(b"name");
        write_varint(&mut buf, 6);
        buf.extend_from_slice(b"String");
        for &s in names {
            write_varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        buf
    }

    /// Helper: one String column body, a varint length plus raw bytes per row.
    fn string_body(values: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        for &s in values {
            write_varint(&mut buf, s.len() as u64);
            buf.extend_from_slice(s.as_bytes());
        }
        buf
    }

    /// Helper: frame one single-column block for a revision >=
    /// DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION: BlockInfo preamble, counts,
    /// column header with the default custom-serialization marker, then `body`.
    fn make_framed_block(name: &str, type_name: &str, num_rows: usize, body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        buf.push(0x00); // is_overflows = false
        write_varint(&mut buf, 2);
        buf.extend_from_slice(&(-1i32).to_le_bytes()); // bucket_num = -1
        write_varint(&mut buf, 0); // BlockInfo terminator
        write_varint(&mut buf, 1); // num_cols
        write_varint(&mut buf, num_rows as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name.as_bytes());
        buf.push(0x00); // default serialization
        buf.extend_from_slice(body);
        buf
    }

    /// Helper: build one String column block with a binary type header
    /// (no framing).
    fn make_binary_string_block(name: &str, values: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, values.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        buf.push(0x15); // DataTypesBinaryEncoding String
        buf.extend(string_body(values));
        buf
    }

    fn framed_options() -> DecodeOptions {
        DecodeOptions {
            protocol_revision: DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION,
            ..DecodeOptions::default()
        }
    }

    /// Feed `data` in `chunk_size` pieces (the whole buffer when zero) through
    /// a decoder from `make`, returning all blocks including `finish()`'s.
    fn decode_chunked_with(
        data: &[u8],
        chunk_size: usize,
        make: impl Fn() -> StreamDecoder,
    ) -> Result<Vec<ColBatch>, DecodeError> {
        let mut dec = make();
        let mut blocks = Vec::new();
        if chunk_size == 0 {
            blocks.extend(dec.feed(data)?);
        } else {
            for chunk in data.chunks(chunk_size) {
                blocks.extend(dec.feed(chunk)?);
            }
        }
        blocks.extend(dec.finish()?);
        Ok(blocks)
    }

    /// Assert that every chunking of `data` decodes to the same blocks as one
    /// whole-buffer feed, and that dropping the final byte fails with
    /// UnexpectedEof at every chunking. Decoders come from `make`.
    fn assert_chunking_parity_with(
        data: &[u8],
        expected_blocks: usize,
        make: impl Fn() -> StreamDecoder,
    ) {
        let whole = decode_chunked_with(data, 0, &make).unwrap();
        assert_eq!(whole.len(), expected_blocks);
        for chunk_size in [1, 7, 64 * 1024] {
            let blocks = decode_chunked_with(data, chunk_size, &make).unwrap();
            assert_eq!(blocks, whole, "chunk size {chunk_size}");
        }
        let truncated = &data[..data.len() - 1];
        for chunk_size in [0, 1, 7, 64 * 1024] {
            let err = decode_chunked_with(truncated, chunk_size, &make).unwrap_err();
            assert!(
                matches!(err, DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof),
                "chunk size {chunk_size}: {err:?}"
            );
        }
    }

    /// Chunking parity with the default text-header decoder.
    fn assert_chunking_parity(data: &[u8], expected_blocks: usize) {
        assert_chunking_parity_with(data, expected_blocks, || {
            StreamDecoder::new(DecodeOptions::default())
        });
    }

    #[test]
    fn test_single_feed_single_block() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let data = make_int64_block("n", &[1, 2, 3]);

        let blocks = dec.feed(&data).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].num_rows, 3);

        let final_blocks = dec.finish().unwrap();
        assert!(final_blocks.is_empty());
    }

    #[test]
    fn test_single_feed_multiple_blocks() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let mut data = make_int64_block("n", &[1, 2]);
        data.extend(make_int64_block("n", &[3, 4, 5]));

        let blocks = dec.feed(&data).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].num_rows, 2);
        assert_eq!(blocks[1].num_rows, 3);
    }

    #[test]
    fn test_incremental_feed_byte_by_byte() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let data = make_int64_block("n", &[42]);

        let mut total_blocks = Vec::new();
        for byte in &data {
            let blocks = dec.feed(&[*byte]).unwrap();
            total_blocks.extend(blocks);
        }
        total_blocks.extend(dec.finish().unwrap());

        assert_eq!(total_blocks.len(), 1);
        assert_eq!(total_blocks[0].num_rows, 1);
        match total_blocks[0].column(0) {
            Column::Int64(col) => assert_eq!(col.values, vec![42]),
            _ => panic!("expected Int64"),
        }
    }

    #[test]
    fn test_incremental_feed_split_mid_block() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let data = make_int64_block("n", &[10, 20, 30]);

        // Split at an arbitrary point mid-block
        let mid = data.len() / 2;

        let blocks1 = dec.feed(&data[..mid]).unwrap();
        assert!(blocks1.is_empty()); // not enough data yet

        let blocks2 = dec.feed(&data[mid..]).unwrap();
        assert_eq!(blocks2.len(), 1);
        assert_eq!(blocks2[0].num_rows, 3);
    }

    #[test]
    fn test_two_blocks_split_across_feeds() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let block1 = make_int64_block("n", &[1, 2]);
        let block2 = make_int64_block("n", &[3, 4]);

        let mut combined = block1.clone();
        combined.extend(&block2);

        // Split mid-way through the second block
        let split = block1.len() + block2.len() / 2;

        let blocks_a = dec.feed(&combined[..split]).unwrap();
        assert_eq!(blocks_a.len(), 1); // first block complete
        assert_eq!(blocks_a[0].num_rows, 2);

        let blocks_b = dec.feed(&combined[split..]).unwrap();
        assert_eq!(blocks_b.len(), 1); // second block complete
        assert_eq!(blocks_b[0].num_rows, 2);
    }

    #[test]
    fn test_schema_mismatch_in_one_feed_errors() {
        // A second block with a different column name is a corrupt payload,
        // same as decode_all_bytes.
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let mut data = make_int64_block("n", &[1, 2]);
        data.extend(make_int64_block("m", &[3]));

        let result = dec.feed(&data);
        assert!(matches!(
            result,
            Err(DecodeError::BlockSchemaMismatch { block_index: 1 })
        ));
    }

    #[test]
    fn test_schema_mismatch_across_feeds_errors() {
        // The first block's schema is retained across feed calls; a later
        // feed with a different column type fails.
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let blocks = dec.feed(&make_int64_block("n", &[1, 2])).unwrap();
        assert_eq!(blocks.len(), 1);

        let result = dec.feed(&make_string_block("n", &["user_1"]));
        assert!(matches!(
            result,
            Err(DecodeError::BlockSchemaMismatch { block_index: 1 })
        ));
    }

    #[test]
    fn test_empty_feed() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let blocks = dec.feed(b"").unwrap();
        assert!(blocks.is_empty());

        let final_blocks = dec.finish().unwrap();
        assert!(final_blocks.is_empty());
    }

    #[test]
    fn test_finish_with_truncated_data_errors() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let data = make_int64_block("n", &[1, 2, 3]);

        // Feed only half the block
        dec.feed(&data[..data.len() / 2]).unwrap();

        // Finish should error — truncated block
        let result = dec.finish();
        assert!(result.is_err());
    }

    #[test]
    fn test_feed_after_finish_errors() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        dec.finish().unwrap();

        let result = dec.feed(b"hello");
        assert!(result.is_err());
    }

    #[test]
    fn test_large_block_incremental() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let values: Vec<i64> = (0..10_000).collect();
        let data = make_int64_block("n", &values);

        // Feed in 1KB chunks
        let mut all_blocks = Vec::new();
        for chunk in data.chunks(1024) {
            all_blocks.extend(dec.feed(chunk).unwrap());
        }
        all_blocks.extend(dec.finish().unwrap());

        assert_eq!(all_blocks.len(), 1);
        assert_eq!(all_blocks[0].num_rows, 10_000);
    }

    #[test]
    fn test_many_small_blocks_streamed() {
        let mut dec = StreamDecoder::new(DecodeOptions::default());

        // Build 100 tiny blocks, concatenate, feed in one go
        let mut data = Vec::new();
        for i in 0..100 {
            data.extend(make_int64_block("n", &[i]));
        }

        let blocks = dec.feed(&data).unwrap();
        assert_eq!(blocks.len(), 100);
        for (i, block) in blocks.iter().enumerate() {
            assert_eq!(block.num_rows, 1);
            match block.column(0) {
                Column::Int64(col) => assert_eq!(col.values[0], i as i64),
                _ => panic!("expected Int64"),
            }
        }
    }

    #[test]
    fn test_string_block_byte_by_byte() {
        // A String block fed one byte at a time must decode once the final byte
        // lands. This exercises the String branch of the completeness scan, which
        // walks the per-value varint length prefixes.
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let data = make_string_block("s", &["user_1", "", "user_2", "13"]);

        let mut total = Vec::new();
        for byte in &data {
            total.extend(dec.feed(&[*byte]).unwrap());
        }
        total.extend(dec.finish().unwrap());

        assert_eq!(total.len(), 1);
        assert_eq!(total[0].num_rows, 4);
        match total[0].column(0) {
            Column::Utf8(c) => {
                assert_eq!(c.value(0), b"user_1");
                assert_eq!(c.value(1), b"");
                assert_eq!(c.value(2), b"user_2");
                assert_eq!(c.value(3), b"13");
            }
            _ => panic!("expected Utf8"),
        }
    }

    #[test]
    fn test_nullable_sum_block_byte_by_byte() {
        // Split through both true flags and their conditional accumulators. The
        // allocation-free boundary scan must wait for the complete final state,
        // then materialize the variable LargeBinary offsets exactly once.
        let mut states = vec![0x00, 0x01];
        states.extend_from_slice(&13u64.to_le_bytes());
        states.push(0x02);
        states.extend_from_slice(&79u64.to_le_bytes());
        let data = make_nullable_sum_block("s", 3, &states);
        let mut dec = StreamDecoder::new(DecodeOptions::default());

        let mut blocks = Vec::new();
        for byte in data {
            blocks.extend(dec.feed(&[byte]).unwrap());
        }
        blocks.extend(dec.finish().unwrap());

        assert_eq!(blocks.len(), 1);
        match blocks[0].column(0) {
            Column::AggregateState(c) => {
                assert_eq!(c.offsets, vec![0, 1, 10, 19]);
                assert_eq!(c.data, states);
            }
            other => panic!("expected AggregateState, got {other:?}"),
        }
    }

    #[test]
    fn test_nullable_sum_true_flag_without_accumulator_stays_truncated() {
        let data = make_nullable_sum_block("s", 2, &[0x00, 0x01]);
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        assert!(dec.feed(&data).unwrap().is_empty());
        assert!(matches!(
            dec.finish(),
            Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn test_nothing_null_block_byte_by_byte() {
        let states = vec![0x00, 0x00, 0x00];
        let data = make_nothing_null_block("s", 3, &states);
        let mut dec = StreamDecoder::new(DecodeOptions::default());

        let mut blocks = Vec::new();
        for byte in data {
            blocks.extend(dec.feed(&[byte]).unwrap());
        }
        blocks.extend(dec.finish().unwrap());

        assert_eq!(blocks.len(), 1);
        match blocks[0].column(0) {
            Column::AggregateState(c) => {
                assert_eq!(c.offsets, vec![0, 1, 2, 3]);
                assert_eq!(c.data, states);
            }
            other => panic!("expected AggregateState, got {other:?}"),
        }
    }

    #[test]
    fn test_nothing_null_short_state_run_stays_truncated() {
        let data = make_nothing_null_block("s", 3, &[0x00, 0x00]);
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        assert!(dec.feed(&data).unwrap().is_empty());
        assert!(matches!(
            dec.finish(),
            Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn test_large_string_block_split_across_many_feeds() {
        // A large String block split into many small chunks must decode exactly
        // once, when the last byte arrives. With the completeness scan in place,
        // the partial feeds never allocate the column buffers.
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let owned: Vec<String> = (0..5_000).map(|i| format!("user_{i}")).collect();
        let values: Vec<&str> = owned.iter().map(String::as_str).collect();
        let data = make_string_block("s", &values);

        let mut all = Vec::new();
        for chunk in data.chunks(64) {
            all.extend(dec.feed(chunk).unwrap());
        }
        all.extend(dec.finish().unwrap());

        assert_eq!(all.len(), 1);
        assert_eq!(all[0].num_rows, 5_000);
        match all[0].column(0) {
            Column::Utf8(c) => {
                assert_eq!(c.value(0), b"user_0");
                assert_eq!(c.value(4_999), b"user_4999");
            }
            _ => panic!("expected Utf8"),
        }
    }

    #[test]
    fn test_high_water_mark_skips_redundant_scan() {
        // White-box check on the high-water mark: a feed that does not complete
        // the block records `scanned == buffer.len()`, and a subsequent feed of
        // zero new bytes must not move past it (no re-scan, no progress).
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let data = make_int64_block("n", &[13, 79, 1]);

        // Feed all but the last byte: the block is incomplete.
        let blocks = dec.feed(&data[..data.len() - 1]).unwrap();
        assert!(blocks.is_empty());
        assert_eq!(dec.scanned, dec.buffer.len());
        let mark = dec.scanned;

        // An empty feed does not grow the buffer, so the scan is skipped and the
        // high-water mark is unchanged.
        let blocks = dec.feed(b"").unwrap();
        assert!(blocks.is_empty());
        assert_eq!(dec.scanned, mark);

        // The final byte completes the block.
        let blocks = dec.feed(&data[data.len() - 1..]).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].num_rows, 3);
        // After consuming the block the mark is reset and the buffer compacted.
        assert_eq!(dec.scanned, 0);
        assert_eq!(dec.pos, 0);
    }

    #[test]
    fn test_unsupported_type_in_complete_stream_errors() {
        // An unsupported type inside an otherwise-complete block surfaces as a
        // DecodeError from the scan, not as "need more bytes". `Decimal64(4)` is
        // a creation-time-only spelling the server never emits on the wire (it
        // always writes the canonical `Decimal(P, S)`), so the parser rejects it
        // and it serves as the unsupported example here.
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let mut data = Vec::new();
        write_varint(&mut data, 1); // num_cols
        write_varint(&mut data, 1); // num_rows
        write_varint(&mut data, 2);
        data.extend_from_slice(b"id");
        let type_name = b"Decimal64(4)";
        write_varint(&mut data, type_name.len() as u64);
        data.extend_from_slice(type_name);
        data.extend_from_slice(&0u64.to_le_bytes()); // any 8 bytes of data

        let result = dec.feed(&data);
        assert!(matches!(result, Err(DecodeError::UnsupportedType { .. })));
    }

    #[test]
    fn test_chunking_parity_string() {
        // 300 rows (2-byte row-count varint) and one >= 128 byte value (2-byte
        // length varint), so the 1-byte chunk sweep splits inside both.
        let long = "x".repeat(200);
        let owned: Vec<String> = (0..300).map(|i| format!("user_{i}")).collect();
        let mut values: Vec<&str> = owned.iter().map(String::as_str).collect();
        values[13] = &long;
        values[79] = "";
        let mut data = make_string_block("s", &values);
        data.extend(make_string_block("s", &["user_300", "seventy nine"]));
        assert_chunking_parity(&data, 2);
    }

    #[test]
    fn test_chunking_parity_nullable_string() {
        // Same multi-byte varint coverage as the plain String case: 300 rows
        // and one >= 128 byte value, with nulls interleaved.
        let long = "y".repeat(150);
        let owned: Vec<String> = (0..300).map(|i| format!("user_{i}")).collect();
        let mut values: Vec<Option<&str>> = owned
            .iter()
            .enumerate()
            .map(|(i, s)| if i % 7 == 0 { None } else { Some(s.as_str()) })
            .collect();
        values[13] = Some(&long);
        values[79] = Some("");
        let mut data = make_nullable_string_block("s", &values);
        data.extend(make_nullable_string_block("s", &[None, Some("13")]));
        assert_chunking_parity(&data, 2);
    }

    #[test]
    fn test_chunking_parity_framed_string() {
        // Revision framing: BlockInfo preamble and the per-column
        // custom-serialization marker byte, split at every byte boundary.
        let mut data = make_framed_block("s", "String", 3, &string_body(&["user_1", "", "user_2"]));
        data.extend(make_framed_block(
            "s",
            "String",
            2,
            &string_body(&["user_3", "13"]),
        ));
        assert_chunking_parity_with(&data, 2, || StreamDecoder::new(framed_options()));
    }

    #[test]
    fn test_chunking_parity_framed_nullable_string() {
        let mut body = vec![0x00, 0x01, 0x00];
        body.extend(string_body(&["user_1", "", "user_2"]));
        let mut data = make_framed_block("s", "Nullable(String)", 3, &body);
        let mut body2 = vec![0x01, 0x00];
        body2.extend(string_body(&["", "13"]));
        data.extend(make_framed_block("s", "Nullable(String)", 2, &body2));
        assert_chunking_parity_with(&data, 2, || StreamDecoder::new(framed_options()));
    }

    #[test]
    fn test_chunking_parity_binary_types_string() {
        // The String parity corpus again, with binary type headers, covering
        // the binary-descriptor resume path.
        let long = "z".repeat(160);
        let owned: Vec<String> = (0..300).map(|i| format!("user_{i}")).collect();
        let mut values: Vec<&str> = owned.iter().map(String::as_str).collect();
        values[13] = &long;
        values[79] = "";
        let mut data = make_binary_string_block("s", &values);
        data.extend(make_binary_string_block("s", &["user_300", ""]));
        assert_chunking_parity_with(&data, 2, || {
            StreamDecoder::new_binary_types(DecodeOptions::default())
        });
    }

    #[test]
    fn test_chunking_parity_low_cardinality_string() {
        let mut data = make_lc_string_block("lc", &["", "user_1", "user_2"], &[1, 2, 1, 0]);
        data.extend(make_lc_string_block("lc", &["", "user_3"], &[1, 1]));
        assert_chunking_parity(&data, 2);
    }

    #[test]
    fn test_chunking_parity_array_string() {
        let mut data = make_array_string_block("a", &[&["user_1", "user_2"], &[], &["13"]]);
        data.extend(make_array_string_block("a", &[&["user_3"]]));
        assert_chunking_parity(&data, 2);
    }

    #[test]
    fn test_chunking_parity_mixed_columns() {
        let mut data = make_mixed_block(&[13, 79], &["user_1", "user_2"]);
        data.extend(make_mixed_block(&[80], &["user_3"]));
        assert_chunking_parity(&data, 2);
    }

    #[test]
    fn test_chunking_parity_zero_row_block() {
        // A zero-row block (headers only, no column data) followed by a
        // row-bearing block.
        let mut data = make_string_block("s", &[]);
        data.extend(make_string_block("s", &["user_1", "user_2"]));
        assert_chunking_parity(&data, 2);
    }

    #[test]
    fn test_string_scan_walks_linear_bytes_across_small_feeds() {
        // Linearity guard: a large String block fed in small chunks must be
        // walked a bounded number of times overall, not re-walked from the
        // block start on every feed.
        let owned: Vec<String> = (0..30_000).map(|i| format!("user_{i}")).collect();
        let values: Vec<&str> = owned.iter().map(String::as_str).collect();
        let data = make_string_block("s", &values);

        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let mut blocks = Vec::new();
        for chunk in data.chunks(256) {
            blocks.extend(dec.feed(chunk).unwrap());
        }
        blocks.extend(dec.finish().unwrap());

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].num_rows, 30_000);
        assert!(
            dec.scan.bytes_walked < 3 * data.len() as u64,
            "scan walked {} bytes for a {}-byte block",
            dec.scan.bytes_walked,
            data.len()
        );
    }

    #[test]
    fn test_binary_type_header_streaming() {
        let mut data = Vec::new();
        write_varint(&mut data, 1);
        write_varint(&mut data, 1);
        write_varint(&mut data, 1);
        data.push(b'v');
        data.push(0x04); // DataTypesBinaryEncoding UInt64
        data.extend_from_slice(&79u64.to_le_bytes());

        let mut decoder = StreamDecoder::new_binary_types(DecodeOptions::default());
        let split = data.len() - 1;
        assert!(decoder.feed(&data[..split]).unwrap().is_empty());
        let blocks = decoder.feed(&data[split..]).unwrap();
        assert_eq!(blocks.len(), 1);
        match blocks[0].column(0) {
            Column::UInt64(column) => assert_eq!(column.values, vec![79]),
            other => panic!("expected UInt64, got {other:?}"),
        }
    }

    #[test]
    fn test_unsupported_protocol_revision_rejected_by_streaming_scans() {
        let options = || DecodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION + 1,
            ..DecodeOptions::default()
        };
        for mut decoder in [
            StreamDecoder::new(options()),
            StreamDecoder::new_binary_types(options()),
        ] {
            assert!(matches!(
                decoder.feed(&[0]),
                Err(DecodeError::UnsupportedProtocolRevision {
                    revision,
                    max_supported: DBMS_TCP_PROTOCOL_VERSION,
                }) if revision == DBMS_TCP_PROTOCOL_VERSION + 1
            ));
        }
    }

    #[test]
    fn test_unsupported_protocol_revision_rejected_for_empty_stream() {
        let options = || DecodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION + 1,
            ..DecodeOptions::default()
        };
        for mut decoder in [
            StreamDecoder::new(options()),
            StreamDecoder::new_binary_types(options()),
        ] {
            assert!(decoder.feed(&[]).unwrap().is_empty());
            assert!(matches!(
                decoder.finish(),
                Err(DecodeError::UnsupportedProtocolRevision {
                    revision,
                    max_supported: DBMS_TCP_PROTOCOL_VERSION,
                }) if revision == DBMS_TCP_PROTOCOL_VERSION + 1
            ));
        }
    }
}
