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
use crate::native::decode::{block_end, decode_next_block, DecodeError, DecodeOptions};
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
    options: DecodeOptions,
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
            options,
            finished: false,
            schema: None,
            blocks_seen: 0,
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
    /// Each iteration first runs the allocation-free [`block_end`] completeness
    /// scan over the unconsumed bytes. Only when it confirms a whole block is
    /// buffered do we run the allocating [`decode_next_block`]. A block that
    /// arrives over several feeds therefore allocates its column buffers exactly
    /// once, when the last byte lands, instead of allocating and discarding them
    /// on every partial feed.
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

            match block_end(data, &self.options) {
                Ok(Some(end)) => {
                    // A full block is buffered. The allocating decode now reads
                    // exactly `data[..end]`; completeness was just verified with
                    // the same framing, so it cannot hit EOF.
                    let mut reader = ByteReader::new(&data[..end]);
                    match decode_next_block(&mut reader, &self.options)? {
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
    use crate::native::varint::write_varint;

    /// Helper: build a Native format block with one Int64 column.
    fn make_int64_block(name: &str, values: &[i64]) -> Vec<u8> {
        let mut buf = Vec::new();
        // num_cols = 1
        write_varint(&mut buf, 1).unwrap();
        // num_rows
        write_varint(&mut buf, values.len() as u64).unwrap();
        // column header
        let name_bytes = name.as_bytes();
        write_varint(&mut buf, name_bytes.len() as u64).unwrap();
        buf.extend_from_slice(name_bytes);
        let type_name = b"Int64";
        write_varint(&mut buf, type_name.len() as u64).unwrap();
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
        write_varint(&mut buf, 1).unwrap(); // num_cols
        write_varint(&mut buf, values.len() as u64).unwrap(); // num_rows
        write_varint(&mut buf, name.len() as u64).unwrap();
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"String";
        write_varint(&mut buf, type_name.len() as u64).unwrap();
        buf.extend_from_slice(type_name);
        for &s in values {
            write_varint(&mut buf, s.len() as u64).unwrap();
            buf.extend_from_slice(s.as_bytes());
        }
        buf
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
        // DecodeError from the scan, not as "need more bytes".
        let mut dec = StreamDecoder::new(DecodeOptions::default());
        let mut data = Vec::new();
        write_varint(&mut data, 1).unwrap(); // num_cols
        write_varint(&mut data, 1).unwrap(); // num_rows
        write_varint(&mut data, 2).unwrap();
        data.extend_from_slice(b"id");
        write_varint(&mut data, 4).unwrap();
        data.extend_from_slice(b"UUID");
        data.extend_from_slice(&0u32.to_le_bytes()); // any 4 bytes of data

        let result = dec.feed(&data);
        assert!(matches!(result, Err(DecodeError::UnsupportedType { .. })));
    }
}
