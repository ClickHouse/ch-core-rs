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
    block_end_binary_types_resume, block_end_resume, decode_scanned_block,
    scan_block_binary_types_resume, scan_block_resume, DecodeError, DecodeOptions, ScanProgress,
    DBMS_TCP_PROTOCOL_VERSION,
};
use crate::native::varint::ByteReader;
use crate::schema::Schema;

/// Result of scanning the bytes at the front of an outer protocol buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum BlockBoundary {
    /// The supplied bytes do not yet contain a complete Native block.
    NeedMore,
    /// One complete block occupies exactly `data[..consumed]`.
    Complete {
        /// Number of bytes occupied by the block.
        consumed: usize,
        /// Number of columns declared in the block header.
        num_columns: usize,
        /// Number of rows declared in the block header.
        num_rows: usize,
    },
}

/// Scan-only boundary finder for one Native block embedded in an outer protocol.
///
/// The caller retains the bytes. After [`BlockBoundary::NeedMore`], the next
/// call must pass the same prefix plus any newly received suffix. The scanner
/// checkpoints verified top-level columns and, inside a column, any body
/// composed from String, fixed-width values, Nullable, Array, Map, Tuple,
/// LowCardinality, Variant, Dynamic, JSON, and AggregateFunction. Runtime
/// type-table prefixes and nested bodies retain progress, so feeding a block
/// in pieces walks it in amortized linear time. A complete block or decode
/// error resets the checkpoint for the next block.
///
/// The scanner creates no decoded column buffers and copies no wire bytes.
/// Header and type parsing can allocate metadata values. The supplied
/// [`DecodeOptions`] apply while validating framing, headers, declared sizes,
/// and column bodies.
///
/// Scan and decode agree on acceptance or rejection and complete-block
/// boundaries, not on error wording or when corruption is reported. On a
/// truncated stream, an invalid Dynamic index or AggregateFunction zero-state
/// byte can be rejected as invalid data before decode reports truncation.
#[derive(Debug)]
pub struct BlockScanner {
    progress: ScanProgress,
    options: DecodeOptions,
    binary_types: bool,
    scanned: usize,
}

impl BlockScanner {
    /// Construct a scanner for Native blocks with textual type headers.
    pub fn new(options: DecodeOptions) -> Self {
        Self {
            progress: ScanProgress::default(),
            options,
            binary_types: false,
            scanned: 0,
        }
    }

    /// Construct a scanner for Native blocks with binary type descriptors.
    pub fn new_binary_types(options: DecodeOptions) -> Self {
        Self {
            binary_types: true,
            ..Self::new(options)
        }
    }

    /// Scan the first Native block in `data`.
    ///
    /// Bytes after a complete block are ignored and remain available to the
    /// outer protocol. Calling this method again after completion starts a new
    /// block. If a caller abandons an incomplete block, call [`Self::reset`]
    /// before scanning unrelated bytes.
    pub fn scan(&mut self, data: &[u8]) -> Result<BlockBoundary, DecodeError> {
        if self.scanned != 0 && data.len() <= self.scanned {
            if data.len() == self.scanned {
                return Ok(BlockBoundary::NeedMore);
            }
            self.reset();
        }

        let result = if self.binary_types {
            scan_block_binary_types_resume(data, &self.options, &mut self.progress)
        } else {
            scan_block_resume(data, &self.options, &mut self.progress)
        };

        match result {
            Ok(Some(block)) => {
                self.scanned = 0;
                Ok(BlockBoundary::Complete {
                    consumed: block.end,
                    num_columns: block.num_columns,
                    num_rows: block.num_rows,
                })
            }
            Ok(None) => {
                self.scanned = data.len();
                Ok(BlockBoundary::NeedMore)
            }
            Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                self.scanned = data.len();
                Ok(BlockBoundary::NeedMore)
            }
            Err(error) => {
                self.scanned = 0;
                Err(error)
            }
        }
    }

    /// Discard progress for an incomplete block.
    pub fn reset(&mut self) {
        self.progress = ScanProgress::default();
        self.scanned = 0;
    }
}

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
    /// Each iteration first runs the scan-only [`block_end_resume`]
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
                            self.accept_block(&batch)?;
                            blocks.push(batch);
                        }
                        // The scan returned `Some`, so a block is present.
                        None => unreachable!("the scan confirmed a complete block"),
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
        self.compact();

        Ok(blocks)
    }

    fn accept_block(&mut self, block: &ColBatch) -> Result<(), DecodeError> {
        match &self.schema {
            None => self.schema = Some(block.schema.clone()),
            Some(first) if block.schema != *first => {
                return Err(DecodeError::BlockSchemaMismatch {
                    block_index: self.blocks_seen,
                });
            }
            Some(_) => {}
        }
        self.blocks_seen += 1;
        Ok(())
    }

    fn compact(&mut self) {
        if self.pos > 0 {
            self.buffer.drain(..self.pos);
            self.scanned = self.scanned.saturating_sub(self.pos);
            self.pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::Column;
    use crate::native::decode::{
        block_end, decode_next_block, decode_next_block_binary_types,
        DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION,
    };
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

    /// Helper: build one Map(String, String) column block (no framing).
    fn make_map_string_block(name: &str, rows: &[&[(&str, &str)]]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, rows.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"Map(String, String)";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        let mut end = 0u64;
        for row in rows {
            end += row.len() as u64;
            buf.extend_from_slice(&end.to_le_bytes());
        }
        for row in rows {
            for (key, _) in *row {
                write_varint(&mut buf, key.len() as u64);
                buf.extend_from_slice(key.as_bytes());
            }
        }
        for row in rows {
            for (_, value) in *row {
                write_varint(&mut buf, value.len() as u64);
                buf.extend_from_slice(value.as_bytes());
            }
        }
        buf
    }

    /// Helper: build one Tuple(Int64, String) column block (no framing).
    fn make_tuple_int_string_block(name: &str, rows: &[(i64, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, rows.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"Tuple(Int64, String)";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        for (value, _) in rows {
            buf.extend_from_slice(&value.to_le_bytes());
        }
        for (_, value) in rows {
            write_varint(&mut buf, value.len() as u64);
            buf.extend_from_slice(value.as_bytes());
        }
        buf
    }

    /// Helper: build one Array(Nullable(String)) column block (no framing).
    fn make_array_nullable_string_block(name: &str, rows: &[&[Option<&str>]]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1);
        write_varint(&mut buf, rows.len() as u64);
        write_varint(&mut buf, name.len() as u64);
        buf.extend_from_slice(name.as_bytes());
        let type_name = b"Array(Nullable(String))";
        write_varint(&mut buf, type_name.len() as u64);
        buf.extend_from_slice(type_name);
        let mut end = 0u64;
        for row in rows {
            end += row.len() as u64;
            buf.extend_from_slice(&end.to_le_bytes());
        }
        for row in rows {
            for value in *row {
                buf.push(u8::from(value.is_none()));
            }
        }
        for row in rows {
            for value in *row {
                let value = value.unwrap_or("");
                write_varint(&mut buf, value.len() as u64);
                buf.extend_from_slice(value.as_bytes());
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

    /// Assert a single block remains incomplete at every proper byte prefix
    /// and completes at its exact final byte.
    fn assert_block_scanner_bytewise(data: &[u8]) {
        assert_eq!(
            block_end(data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
        let mut scanner = BlockScanner::new(DecodeOptions::default());
        for end in 0..data.len() {
            assert_eq!(
                scanner.scan(&data[..end]).unwrap(),
                BlockBoundary::NeedMore,
                "prefix length {end}"
            );
        }
        assert!(matches!(
            scanner.scan(data).unwrap(),
            BlockBoundary::Complete { consumed, .. } if consumed == data.len()
        ));
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
        // scan-only boundary walk must wait for the complete final state,
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
    fn block_scanner_stops_before_outer_protocol_bytes() {
        let block = make_int64_block("n", &[13, 79]);
        let mut chunk = block.clone();
        chunk.extend(std::iter::repeat(0xa5).take(1024 * 1024));
        let mut scanner = BlockScanner::new(DecodeOptions::default());

        assert_eq!(scanner.scan(&chunk[..3]).unwrap(), BlockBoundary::NeedMore);
        assert_eq!(
            scanner.scan(&chunk).unwrap(),
            BlockBoundary::Complete {
                consumed: block.len(),
                num_columns: 1,
                num_rows: 2,
            }
        );
    }

    #[test]
    fn block_scanner_handles_adjacent_blocks_without_taking_the_second() {
        let first = make_string_block("s", &["user_1"]);
        let second = make_string_block("s", &["user_2", "user_3"]);
        let joined = [first.as_slice(), second.as_slice()].concat();
        let mut scanner = BlockScanner::new(DecodeOptions::default());

        assert_eq!(
            scanner.scan(&joined).unwrap(),
            BlockBoundary::Complete {
                consumed: first.len(),
                num_columns: 1,
                num_rows: 1,
            }
        );
        assert_eq!(
            scanner.scan(&joined[first.len()..]).unwrap(),
            BlockBoundary::Complete {
                consumed: second.len(),
                num_columns: 1,
                num_rows: 2,
            }
        );
    }

    #[test]
    fn block_scanner_validates_revision_on_empty_input() {
        let options = DecodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION + 1,
            ..DecodeOptions::default()
        };
        let mut scanner = BlockScanner::new(options);

        assert!(matches!(
            scanner.scan(b""),
            Err(DecodeError::UnsupportedProtocolRevision { .. })
        ));
    }

    #[test]
    fn block_scanner_retains_partial_first_block_info_varint() {
        let mut block = make_framed_block("n", "Int64", 1, &13_i64.to_le_bytes());
        // Non-canonical but valid two-byte encoding of BlockInfo field 1.
        block.splice(0..1, [0x81, 0x00]);
        let mut scanner = BlockScanner::new(framed_options());

        assert_eq!(scanner.scan(&block[..1]).unwrap(), BlockBoundary::NeedMore);
        assert_eq!(
            scanner.scan(&block).unwrap(),
            BlockBoundary::Complete {
                consumed: block.len(),
                num_columns: 1,
                num_rows: 1,
            }
        );
    }

    #[test]
    fn block_scanner_rejects_malformed_block_info() {
        let mut scanner = BlockScanner::new(framed_options());

        assert!(matches!(
            scanner.scan(&[0x04, 0x00]),
            Err(DecodeError::InvalidBlockInfo { field_num: 4 })
        ));
    }

    #[test]
    fn block_scanner_retains_partial_revision_zero_column_count() {
        let block = make_int64_block("n", &[13]);
        let mut noncanonical = vec![0x81];
        noncanonical.extend_from_slice(&block[1..]);
        let mut scanner = BlockScanner::new(DecodeOptions::default());

        assert_eq!(
            scanner.scan(&noncanonical[..1]).unwrap(),
            BlockBoundary::NeedMore
        );
        noncanonical.insert(1, 0x00);
        assert_eq!(
            scanner.scan(&noncanonical).unwrap(),
            BlockBoundary::Complete {
                consumed: noncanonical.len(),
                num_columns: 1,
                num_rows: 1,
            }
        );
    }

    #[test]
    fn binary_type_block_scanner_reports_shape_and_boundary() {
        let block = make_binary_string_block("s", &["user_1", "user_2"]);
        let mut with_suffix = block.clone();
        with_suffix.extend_from_slice(&[0xa5; 16]);
        let mut scanner = BlockScanner::new_binary_types(DecodeOptions::default());

        assert_eq!(
            scanner.scan(&with_suffix).unwrap(),
            BlockBoundary::Complete {
                consumed: block.len(),
                num_columns: 1,
                num_rows: 2,
            }
        );
    }

    #[test]
    fn binary_dynamic_runtime_descriptor_scan_matches_decode_at_every_split() {
        let mut block = Vec::new();
        write_varint(&mut block, 1);
        write_varint(&mut block, 3);
        write_varint(&mut block, 1);
        block.push(b'd');
        block.extend([0x2b, 32]); // Dynamic(max_types=32)
        block.extend(2u64.to_le_bytes());
        write_varint(&mut block, 1);
        block.push(0x15); // String runtime descriptor
        block.extend(0u64.to_le_bytes());
        block.extend([1, u8::MAX, 1]);
        block.extend(string_values(&["user_1", "user_2"]));

        let mut reader = ByteReader::new(&block);
        let decoded = decode_next_block_binary_types(&mut reader, &DecodeOptions::default())
            .unwrap()
            .unwrap();
        let expected = BlockBoundary::Complete {
            consumed: reader.position(),
            num_columns: decoded.schema.fields.len(),
            num_rows: decoded.num_rows,
        };
        for split in 0..block.len() {
            let mut scanner = BlockScanner::new_binary_types(DecodeOptions::default());
            assert_eq!(
                scanner.scan(&block[..split]).unwrap(),
                BlockBoundary::NeedMore,
                "split {split}"
            );
            assert_eq!(scanner.scan(&block).unwrap(), expected, "split {split}");
        }
    }

    #[test]
    fn nested_prefix_free_scanner_matches_block_end_bytewise() {
        let array = make_array_string_block("a", &[&["user_1", "user_2"], &[], &["13"]]);
        let map = make_map_string_block(
            "m",
            &[
                &[("user_1", "13"), ("user_2", "79")],
                &[],
                &[("user_3", "")],
            ],
        );
        let tuple = make_tuple_int_string_block("t", &[(13, "user_1"), (79, ""), (-7, "user_2")]);
        let nullable =
            make_array_nullable_string_block("n", &[&[Some("user_1"), None], &[], &[Some("79")]]);

        for data in [&array, &map, &tuple, &nullable] {
            assert_block_scanner_bytewise(data);
        }
    }

    #[test]
    fn nested_array_scan_walks_linear_bytes_across_socket_sized_feeds() {
        let value = "x".repeat(4 * 1024 * 1024);
        let data = make_array_string_block("a", &[&[value.as_str()]]);
        let mut scanner = BlockScanner::new(DecodeOptions::default());

        for end in (64 * 1024..data.len()).step_by(64 * 1024) {
            assert_eq!(scanner.scan(&data[..end]).unwrap(), BlockBoundary::NeedMore);
        }
        assert!(matches!(
            scanner.scan(&data).unwrap(),
            BlockBoundary::Complete { consumed, .. } if consumed == data.len()
        ));
        assert!(
            scanner.progress.bytes_walked < 3 * data.len() as u64,
            "scanner walked {} bytes for a {}-byte nested block",
            scanner.progress.bytes_walked,
            data.len()
        );
    }

    #[test]
    fn nested_array_scanner_preserves_decreasing_offset_error_across_split() {
        let mut data = Vec::new();
        write_varint(&mut data, 1);
        write_varint(&mut data, 2);
        write_varint(&mut data, 1);
        data.push(b'a');
        write_varint(&mut data, 13);
        data.extend_from_slice(b"Array(String)");
        let offset_start = data.len();
        data.extend_from_slice(&2u64.to_le_bytes());
        data.extend_from_slice(&1u64.to_le_bytes());

        let expected = block_end(&data, &DecodeOptions::default()).unwrap_err();
        assert!(matches!(expected, DecodeError::InvalidArray { .. }));

        let mut scanner = BlockScanner::new(DecodeOptions::default());
        assert_eq!(
            scanner
                .scan(&data[..offset_start + std::mem::size_of::<u64>()])
                .unwrap(),
            BlockBoundary::NeedMore
        );
        assert!(matches!(
            scanner.scan(&data),
            Err(DecodeError::InvalidArray { .. })
        ));
    }

    #[test]
    fn impossible_array_offset_run_stays_incomplete_after_more_bytes() {
        let mut data = Vec::new();
        write_varint(&mut data, 1);
        write_varint(&mut data, usize::MAX as u64);
        write_varint(&mut data, 1);
        data.push(b'a');
        write_varint(&mut data, 12);
        data.extend_from_slice(b"Array(Int64)");
        let mut scanner = BlockScanner::new(DecodeOptions::default());

        assert_eq!(scanner.scan(&data).unwrap(), BlockBoundary::NeedMore);
        data.push(0);
        assert_eq!(scanner.scan(&data).unwrap(), BlockBoundary::NeedMore);
        data.push(0);
        assert_eq!(scanner.scan(&data).unwrap(), BlockBoundary::NeedMore);
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

    // -----------------------------------------------------------------------
    // Resumable LowCardinality / Variant scan parity
    // -----------------------------------------------------------------------

    /// One column of a hand-built block: name, type string, and the column's
    /// full data bytes (state-prefix words, then body).
    type TestColumn<'a> = (&'a str, &'a str, Vec<u8>);

    /// Build one block with a BlockInfo preamble when `revision > 0` and the
    /// custom-serialization marker when `revision >= 54454`.
    fn make_block_at(revision: u64, num_rows: usize, columns: &[TestColumn]) -> Vec<u8> {
        let mut buf = Vec::new();
        if revision > 0 {
            write_varint(&mut buf, 1);
            buf.push(0x00);
            write_varint(&mut buf, 2);
            buf.extend_from_slice(&(-1i32).to_le_bytes());
            write_varint(&mut buf, 0);
        }
        write_varint(&mut buf, columns.len() as u64);
        write_varint(&mut buf, num_rows as u64);
        for (name, type_name, data) in columns {
            write_varint(&mut buf, name.len() as u64);
            buf.extend_from_slice(name.as_bytes());
            write_varint(&mut buf, type_name.len() as u64);
            buf.extend_from_slice(type_name.as_bytes());
            if revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION {
                buf.push(0x00);
            }
            buf.extend_from_slice(data);
        }
        buf
    }

    fn string_values(values: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        for value in values {
            write_varint(&mut buf, value.len() as u64);
            buf.extend_from_slice(value.as_bytes());
        }
        buf
    }

    /// One LowCardinality body (no key-version prefix): index word with width
    /// tag `tag`, dictionary size, `dictionary` body bytes, row count, indexes.
    fn lc_body(tag: u64, num_keys: usize, dictionary: &[u8], indexes: &[u64]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(tag | LC_HAS_ADDITIONAL_KEYS_BIT).to_le_bytes());
        buf.extend_from_slice(&(num_keys as u64).to_le_bytes());
        buf.extend_from_slice(dictionary);
        buf.extend_from_slice(&(indexes.len() as u64).to_le_bytes());
        for index in indexes {
            buf.extend_from_slice(&index.to_le_bytes()[..1 << tag]);
        }
        buf
    }

    const LC_KEY_VERSION: [u8; 8] = 1u64.to_le_bytes();

    fn offsets(ends: &[u64]) -> Vec<u8> {
        ends.iter().flat_map(|end| end.to_le_bytes()).collect()
    }

    fn options_at(revision: u64) -> DecodeOptions {
        DecodeOptions {
            protocol_revision: revision,
            ..DecodeOptions::default()
        }
    }

    /// The boundary `decode_next_block` leaves the reader at for the first block
    /// of `data`, in `BlockBoundary` form.
    fn decoded_boundary(data: &[u8], revision: u64) -> BlockBoundary {
        let mut reader = ByteReader::new(data);
        let batch = decode_next_block(&mut reader, &options_at(revision))
            .unwrap()
            .expect("a block");
        BlockBoundary::Complete {
            consumed: reader.position(),
            num_columns: batch.schema.fields.len(),
            num_rows: batch.num_rows,
        }
    }

    /// Scanner parity with `decode_next_block` for the first block of `data`:
    /// one-shot, fed one byte at a time through one scanner, and at every
    /// two-way split (strided above 4 KiB) through a fresh scanner. Bytes after the block are offered
    /// at every step, so a scanner overrunning the boundary would be caught.
    fn assert_scan_matches_decode(data: &[u8], revision: u64) {
        let expected = decoded_boundary(data, revision);
        let BlockBoundary::Complete { consumed: end, .. } = expected else {
            unreachable!()
        };
        assert_eq!(
            BlockScanner::new(options_at(revision)).scan(data).unwrap(),
            expected
        );

        let mut scanner = BlockScanner::new(options_at(revision));
        for prefix in 0..end {
            assert_eq!(
                scanner.scan(&data[..prefix]).unwrap(),
                BlockBoundary::NeedMore,
                "bytewise prefix {prefix}/{end}"
            );
        }
        assert_eq!(scanner.scan(data).unwrap(), expected, "bytewise final");

        // Every split for small blocks; a co-prime stride keeps the 10 KB
        // real-server fixtures fast in debug builds (the release-mode probe
        // covers every split of those).
        let stride = if end > 4096 { 13 } else { 1 };
        for split in (0..end).step_by(stride) {
            let mut scanner = BlockScanner::new(options_at(revision));
            assert_eq!(
                scanner.scan(&data[..split]).unwrap(),
                BlockBoundary::NeedMore,
                "split {split}/{end}"
            );
            assert_eq!(scanner.scan(data).unwrap(), expected, "split {split}/{end}");
        }
    }

    /// Every block of a multi-block stream, in order, through one reused scanner
    /// fed in `step`-byte increments.
    fn assert_stream_scan_matches_decode(data: &[u8], revision: u64, step: usize) {
        let mut scanner = BlockScanner::new(options_at(revision));
        let mut start = 0;
        while start < data.len() {
            let rest = &data[start..];
            let expected = decoded_boundary(rest, revision);
            let BlockBoundary::Complete { consumed, .. } = expected else {
                unreachable!()
            };
            let mut available = step.min(rest.len());
            loop {
                match scanner.scan(&rest[..available]).unwrap() {
                    BlockBoundary::NeedMore => {
                        assert!(available < consumed, "NeedMore with the whole block");
                        available = (available + step).min(rest.len());
                    }
                    complete => {
                        assert_eq!(complete, expected);
                        break;
                    }
                }
            }
            start += consumed;
        }
    }

    /// A scan error that decode would also raise on the same complete bytes.
    fn assert_scan_rejects_like_decode(data: &[u8], split: usize) {
        let decoded =
            decode_next_block(&mut ByteReader::new(data), &DecodeOptions::default()).unwrap_err();
        let mut scanner = BlockScanner::new(DecodeOptions::default());
        let first = scanner.scan(&data[..split]);
        let scanned = match first {
            Ok(BlockBoundary::NeedMore) => scanner.scan(data).unwrap_err(),
            other => other.unwrap_err(),
        };
        assert_eq!(scanned.to_string(), decoded.to_string());
    }

    fn lc_string_column(dictionary: &[&str], indexes: &[u64]) -> Vec<u8> {
        let mut data = LC_KEY_VERSION.to_vec();
        data.extend(lc_body(
            0,
            dictionary.len(),
            &string_values(dictionary),
            indexes,
        ));
        data
    }

    fn dynamic_prefix(version: u64, types: &[&str]) -> Vec<u8> {
        let mut data = version.to_le_bytes().to_vec();
        write_varint(&mut data, types.len() as u64);
        for ch_type in types {
            write_varint(&mut data, ch_type.len() as u64);
            data.extend_from_slice(ch_type.as_bytes());
        }
        if version != 3 {
            data.extend_from_slice(&0u64.to_le_bytes());
        }
        data
    }

    #[test]
    fn runtime_shaped_columns_scan_match_decode_at_every_split() {
        let mut dynamic = dynamic_prefix(2, &["String"]);
        dynamic.extend([1, u8::MAX, 1]);
        dynamic.extend(string_values(&["user_1", "user_2"]));

        let mut json = 3u64.to_le_bytes().to_vec();
        write_varint(&mut json, 0);

        let mut json_text = 1u64.to_le_bytes().to_vec();
        json_text.extend(string_values(&["{}", "13", "null"]));

        let mut json_in_dynamic = dynamic_prefix(3, &["JSON"]);
        json_in_dynamic.extend(3u64.to_le_bytes());
        write_varint(&mut json_in_dynamic, 0);
        json_in_dynamic.push(0);

        let mut array_dynamic = dynamic_prefix(2, &["UInt64"]);
        array_dynamic.extend(offsets(&[1, 1, 3]));
        array_dynamic.extend([0, u8::MAX, 0]);
        array_dynamic.extend(13u64.to_le_bytes());
        array_dynamic.extend(79u64.to_le_bytes());

        let mut tuple_dynamic = dynamic_prefix(2, &["String"]);
        tuple_dynamic.extend([1, u8::MAX, 1]);
        tuple_dynamic.extend(string_values(&["x", "z"]));
        tuple_dynamic.extend([7, 8, 9]);

        let mut nullable_json = 3u64.to_le_bytes().to_vec();
        write_varint(&mut nullable_json, 0);
        nullable_json.extend([0, 1, 0]);

        for (rows, name, ch_type, body) in [
            (3, "d", "Dynamic(max_types=2)", dynamic),
            (3, "j", "JSON", json),
            (3, "jt", "JSON", json_text),
            (1, "dj", "Dynamic", json_in_dynamic),
            (3, "a", "Array(Dynamic(max_types=1))", array_dynamic),
            (3, "t", "Tuple(Dynamic(max_types=1), UInt8)", tuple_dynamic),
            (3, "n", "Nullable(JSON)", nullable_json),
            (3, "g", "AggregateFunction(count)", vec![0, 13, 79]),
            (
                3,
                "s",
                "AggregateFunction(sum, Nullable(UInt8))",
                vec![0, 1, 13, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
        ] {
            assert_scan_matches_decode(&make_block_at(0, rows, &[(name, ch_type, body)]), 0);
        }
    }

    #[test]
    fn runtime_prefix_and_aggregate_malformed_parity_at_every_split() {
        let duplicate = dynamic_prefix(2, &["String", "String"]);
        let mut unsorted_json = 3u64.to_le_bytes().to_vec();
        write_varint(&mut unsorted_json, 2);
        for path in ["z", "a"] {
            write_varint(&mut unsorted_json, path.len() as u64);
            unsorted_json.extend_from_slice(path.as_bytes());
        }
        let cases = [
            make_block_at(0, 1, &[("d", "Dynamic", duplicate)]),
            make_block_at(0, 1, &[("j", "JSON", unsorted_json)]),
            make_block_at(
                0,
                1,
                &[(
                    "g",
                    "AggregateFunction(nothingNull, Nullable(Nothing))",
                    vec![1],
                )],
            ),
        ];
        for data in cases {
            for split in 0..=data.len() {
                assert_scan_rejects_like_decode(&data, split);
            }
        }
    }

    #[test]
    fn flattened_json_scan_preserves_cumulative_allocation_budget() {
        let options = || DecodeOptions {
            max_synthetic_allocation_bytes: 32,
            ..DecodeOptions::default()
        };
        let mut nested = 3u64.to_le_bytes().to_vec();
        write_varint(&mut nested, 0);
        nested.extend(3u64.to_le_bytes());
        write_varint(&mut nested, 0);
        let nested = make_block_at(0, 3, &[("j", "JSON(a JSON)", nested)]);
        let decoded = decode_next_block(&mut ByteReader::new(&nested), &options()).unwrap_err();
        for split in 0..=nested.len() {
            let mut scanner = BlockScanner::new(options());
            if split != nested.len() {
                assert_eq!(
                    scanner.scan(&nested[..split]).unwrap(),
                    BlockBoundary::NeedMore
                );
            }
            let scanned = scanner.scan(&nested).unwrap_err();
            assert_eq!(scanned.to_string(), decoded.to_string(), "split {split}");
        }

        let mut pathless = 3u64.to_le_bytes().to_vec();
        write_varint(&mut pathless, 0);
        let pathless = make_block_at(0, 3, &[("j", "JSON", pathless)]);
        let mut scanner = BlockScanner::new(options());
        assert!(matches!(
            scanner.scan(&pathless).unwrap(),
            BlockBoundary::Complete { consumed, .. } if consumed == pathless.len()
        ));
    }

    #[test]
    fn runtime_tables_and_json_bodies_scan_linearly_across_small_feeds() {
        let type_names: Vec<String> = (1..=1_000)
            .map(|width| format!("FixedString({width})"))
            .collect();
        let type_refs: Vec<&str> = type_names.iter().map(String::as_str).collect();
        let rows = 10_000;
        let mut dynamic = dynamic_prefix(3, &type_refs);
        for _ in 0..rows {
            dynamic.extend_from_slice(&1_000u16.to_le_bytes());
        }
        let dynamic = make_block_at(0, rows, &[("d", "Dynamic", dynamic)]);

        let values = vec!["x".repeat(40); 30_000];
        let value_refs: Vec<&str> = values.iter().map(String::as_str).collect();
        let mut json = 3u64.to_le_bytes().to_vec();
        write_varint(&mut json, 0);
        json.extend(string_values(&value_refs));
        let json = make_block_at(0, value_refs.len(), &[("j", "JSON(s String)", json)]);

        for (type_name, data) in [("Dynamic", dynamic), ("JSON", json)] {
            let mut scanner = BlockScanner::new(DecodeOptions::default());
            for end in (128..data.len()).step_by(128) {
                assert_eq!(scanner.scan(&data[..end]).unwrap(), BlockBoundary::NeedMore);
            }
            assert!(matches!(
                scanner.scan(&data).unwrap(),
                BlockBoundary::Complete { consumed, .. } if consumed == data.len()
            ));
            assert!(
                scanner.progress.bytes_walked < 2 * data.len() as u64,
                "{type_name}: scanner walked {} bytes for {} input bytes",
                scanner.progress.bytes_walked,
                data.len()
            );
        }
    }

    #[test]
    fn many_json_paths_keep_retained_task_memory_bounded() {
        const PATHS: usize = 80_000;
        let mut json = 3u64.to_le_bytes().to_vec();
        write_varint(&mut json, PATHS as u64);
        for path in 0..PATHS {
            let path = format!("path_{path:05}");
            write_varint(&mut json, path.len() as u64);
            json.extend_from_slice(path.as_bytes());
        }
        let names_len = json.len();
        for _ in 0..PATHS {
            json.extend_from_slice(&3u64.to_le_bytes());
            write_varint(&mut json, 0);
        }
        let prefixes_len = json.len();
        json.extend(std::iter::repeat(0u8).take(PATHS));
        let header_len = make_block_at(0, 1, &[("j", "JSON", Vec::new())]).len();
        let data = make_block_at(0, 1, &[("j", "JSON", json)]);
        assert!(data.len() > 1_400_000);

        let mut scanner = BlockScanner::new(DecodeOptions::default());
        let names_end = header_len + names_len;
        for end in (4096..names_end).step_by(4096) {
            assert_eq!(scanner.scan(&data[..end]).unwrap(), BlockBoundary::NeedMore);
        }
        let prefixes_end = header_len + prefixes_len;
        let body_mid = prefixes_end + PATHS / 2;
        let mut retained = Vec::new();
        for end in [names_end, prefixes_end, body_mid] {
            assert_eq!(scanner.scan(&data[..end]).unwrap(), BlockBoundary::NeedMore);
            assert!(scanner.progress.retained_tasks_have_empty_payloads());
            retained.push(scanner.progress.retained_task_bytes());
        }
        assert!(matches!(
            scanner.scan(&data).unwrap(),
            BlockBoundary::Complete { consumed, .. } if consumed == data.len()
        ));
        assert!(
            retained.iter().all(|&bytes| bytes <= 4 * 1024),
            "retained task stacks used {retained:?} bytes"
        );
    }

    #[test]
    fn runtime_shaped_zero_rows_and_adjacent_blocks_reset_scan_state() {
        for ch_type in [
            "Dynamic",
            "JSON",
            "Array(Dynamic)",
            "Tuple(Dynamic, UInt8)",
            "Nullable(JSON)",
            "AggregateFunction(count)",
        ] {
            assert_scan_matches_decode(&make_block_at(0, 0, &[("c", ch_type, Vec::new())]), 0);
        }

        let mut dynamic = dynamic_prefix(2, &["String"]);
        dynamic.push(1);
        dynamic.extend(string_values(&["user_1"]));
        let dynamic = make_block_at(0, 1, &[("c", "Dynamic", dynamic)]);

        let mut json = 1u64.to_le_bytes().to_vec();
        json.extend(string_values(&["{}"]));
        let json = make_block_at(0, 1, &[("c", "JSON", json)]);

        let aggregate = make_block_at(0, 1, &[("c", "AggregateFunction(count)", vec![13])]);
        for block in [dynamic, json, aggregate] {
            let stream = [block.clone(), block].concat();
            for step in [1, 7, 19] {
                assert_stream_scan_matches_decode(&stream, 0, step);
            }
        }
    }

    #[test]
    fn low_cardinality_scan_matches_decode_at_every_split() {
        let plain = lc_string_column(&["", "user_1", "user_2"], &[1, 2, 1, 0]);
        // Nullable dictionary: index 0 is the NULL sentinel, u16 indexes.
        let mut nullable = LC_KEY_VERSION.to_vec();
        nullable.extend(lc_body(
            1,
            3,
            &string_values(&["", "user_1", "user_2"]),
            &[0, 2, 1, 0],
        ));
        // Fixed-width dictionary with u32 indexes.
        let mut uint32 = LC_KEY_VERSION.to_vec();
        let dictionary: Vec<u8> = [0u32, 13, 79]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        uint32.extend(lc_body(2, 3, &dictionary, &[2, 1, 1, 0]));
        // u64 indexes.
        let mut wide = LC_KEY_VERSION.to_vec();
        wide.extend(lc_body(3, 2, &string_values(&["", "x"]), &[1, 1, 0, 1]));

        for revision in [0, 54460, 54485] {
            for column in [
                ("s", "LowCardinality(String)", plain.clone()),
                ("n", "LowCardinality(Nullable(String))", nullable.clone()),
                ("u", "LowCardinality(UInt32)", uint32.clone()),
                ("w", "LowCardinality(String)", wide.clone()),
            ] {
                assert_scan_matches_decode(&make_block_at(revision, 4, &[column]), revision);
            }
            // All four in one block, with trailing bytes after it.
            let mut block = make_block_at(
                revision,
                4,
                &[
                    ("s", "LowCardinality(String)", plain.clone()),
                    ("n", "LowCardinality(Nullable(String))", nullable.clone()),
                    ("u", "LowCardinality(UInt32)", uint32.clone()),
                    ("w", "LowCardinality(String)", wide.clone()),
                ],
            );
            block.extend_from_slice(&[0xa5; 9]);
            assert_scan_matches_decode(&block, revision);
        }
    }

    #[test]
    fn low_cardinality_in_containers_scan_matches_decode_at_every_split() {
        // Array(LowCardinality(String)): [["a","b"], [], ["a"]].
        let mut array = LC_KEY_VERSION.to_vec();
        array.extend(offsets(&[2, 2, 3]));
        array.extend(lc_body(0, 3, &string_values(&["", "a", "b"]), &[1, 2, 1]));
        // Every array empty: the key version is still in the column prefix, but
        // the zero-length LowCardinality run has no body at all.
        let mut empty_array = LC_KEY_VERSION.to_vec();
        empty_array.extend(offsets(&[0, 0, 0]));
        // Map(LowCardinality(String), String).
        let mut map = LC_KEY_VERSION.to_vec();
        map.extend(offsets(&[1, 3, 3]));
        map.extend(lc_body(0, 3, &string_values(&["", "k1", "k2"]), &[1, 1, 2]));
        map.extend(string_values(&["v1", "", "v3"]));
        // Tuple(LowCardinality(String), UInt8, LowCardinality(Nullable(String))):
        // both key versions lead the column, then the element bodies in order.
        let mut tuple = LC_KEY_VERSION.to_vec();
        tuple.extend(LC_KEY_VERSION);
        tuple.extend(lc_body(0, 2, &string_values(&["", "t"]), &[1, 0, 1]));
        tuple.extend([7, 8, 9]);
        tuple.extend(lc_body(0, 2, &string_values(&["", "z"]), &[0, 1, 0]));
        // Array(Tuple(LowCardinality(String), Array(LowCardinality(String)))).
        let mut nested = LC_KEY_VERSION.to_vec();
        nested.extend(LC_KEY_VERSION);
        nested.extend(offsets(&[1, 1, 3]));
        nested.extend(lc_body(0, 2, &string_values(&["", "o"]), &[1, 1, 0]));
        nested.extend(offsets(&[2, 2, 3]));
        nested.extend(lc_body(1, 2, &string_values(&["", "i"]), &[1, 0, 1]));
        // Variant(LowCardinality(String), String, UInt64): mode word, then the
        // LowCardinality key version, discriminators, dense alternative runs.
        let mut variant = 0u64.to_le_bytes().to_vec();
        variant.extend(LC_KEY_VERSION);
        variant.extend([0, 255, 2]);
        variant.extend(lc_body(0, 2, &string_values(&["", "lc"]), &[1]));
        variant.extend(13u64.to_le_bytes());
        // Map(String, Variant(String, UInt64)) with an alternative that never
        // occurs. The mode word is a prefix word, so it leads the column.
        let mut map_variant = 0u64.to_le_bytes().to_vec();
        map_variant.extend(offsets(&[1, 1, 2]));
        map_variant.extend(string_values(&["k", "l"]));
        map_variant.extend([1, 255]);
        map_variant.extend(79u64.to_le_bytes());

        let columns: [TestColumn; 7] = [
            ("a", "Array(LowCardinality(String))", array),
            ("e", "Array(LowCardinality(String))", empty_array),
            ("m", "Map(LowCardinality(String), String)", map),
            (
                "t",
                "Tuple(LowCardinality(String), UInt8, LowCardinality(Nullable(String)))",
                tuple,
            ),
            (
                "x",
                "Array(Tuple(LowCardinality(String), Array(LowCardinality(String))))",
                nested,
            ),
            (
                "v",
                "Variant(LowCardinality(String), String, UInt64)",
                variant,
            ),
            ("mv", "Map(String, Variant(String, UInt64))", map_variant),
        ];
        for revision in [0, 54460] {
            for column in &columns {
                assert_scan_matches_decode(
                    &make_block_at(revision, 3, std::slice::from_ref(column)),
                    revision,
                );
            }
            assert_scan_matches_decode(&make_block_at(revision, 3, &columns), revision);
        }
    }

    #[test]
    fn low_cardinality_multi_block_stream_scan_matches_decode() {
        let first = make_block_at(
            54460,
            2,
            &[(
                "s",
                "LowCardinality(String)",
                lc_string_column(&["", "a"], &[1, 0]),
            )],
        );
        let second = make_block_at(
            54460,
            3,
            &[(
                "s",
                "LowCardinality(String)",
                lc_string_column(&["", "b", "c"], &[2, 1, 2]),
            )],
        );
        // A zero-row block carries the header only: no key version, no body.
        let empty = make_block_at(54460, 0, &[("s", "LowCardinality(String)", Vec::new())]);
        let stream = [first, second, empty].concat();
        for step in [1, 3, 8, 17, stream.len()] {
            assert_stream_scan_matches_decode(&stream, 54460, step);
        }
        let zero_rows = make_block_at(0, 0, &[("s", "LowCardinality(String)", Vec::new())]);
        assert_scan_matches_decode(&zero_rows, 0);
    }

    #[test]
    fn low_cardinality_scan_rejects_what_decode_rejects_across_splits() {
        let block = |data: Vec<u8>| make_block_at(0, 2, &[("s", "LowCardinality(String)", data)]);
        let mut bad_key_version = 2u64.to_le_bytes().to_vec();
        bad_key_version.extend(lc_body(0, 2, &string_values(&["", "a"]), &[1, 0]));
        let mut bad_flags = LC_KEY_VERSION.to_vec();
        bad_flags.extend(lc_body(0, 2, &string_values(&["", "a"]), &[1, 0]));
        bad_flags[8 + 1] = 0; // clear HasAdditionalKeysBit
        let mut bad_tag = LC_KEY_VERSION.to_vec();
        bad_tag.extend(lc_body(0, 2, &string_values(&["", "a"]), &[1, 0]));
        bad_tag[8] = 4;
        let mut bad_rows = LC_KEY_VERSION.to_vec();
        bad_rows.extend(lc_body(0, 2, &string_values(&["", "a"]), &[1, 0]));
        let rows_at = bad_rows.len() - 2 - 8;
        bad_rows[rows_at] = 3;
        let out_of_range = lc_string_column(&["", "a"], &[1, 2]);

        for data in [
            block(bad_key_version),
            block(bad_flags),
            block(bad_tag),
            block(bad_rows),
            block(out_of_range),
        ] {
            for split in 0..=data.len() {
                assert_scan_rejects_like_decode(&data, split);
            }
        }

        // A Variant discriminator naming no alternative.
        let mut variant = 0u64.to_le_bytes().to_vec();
        variant.extend([0, 2]);
        variant.extend(string_values(&["s"]));
        let data = make_block_at(0, 2, &[("v", "Variant(String, UInt64)", variant)]);
        for split in 0..=data.len() {
            assert_scan_rejects_like_decode(&data, split);
        }
    }

    /// A String length past `i32::MAX` is fatal `InvalidData` in decode before
    /// any payload is read; the scan must reject it the same way at every
    /// split rather than wait forever for 2 GiB of payload.
    #[test]
    fn oversized_string_length_rejected_like_decode_at_every_split() {
        let oversized = [0x80, 0x80, 0x80, 0x80, 0x08]; // varint 2^31
        let string = make_block_at(0, 1, &[("c", "String", oversized.to_vec())]);
        let mut array = offsets(&[1]);
        array.extend(oversized);
        let array = make_block_at(0, 1, &[("a", "Array(String)", array)]);
        let mut lc = LC_KEY_VERSION.to_vec();
        lc.extend((LC_HAS_ADDITIONAL_KEYS_BIT).to_le_bytes());
        lc.extend(1u64.to_le_bytes());
        lc.extend(oversized);
        let lc = make_block_at(0, 1, &[("l", "LowCardinality(String)", lc)]);
        for data in [string, array, lc] {
            let err = block_end(&data, &DecodeOptions::default()).unwrap_err();
            assert!(
                matches!(err, DecodeError::Io(ref e) if e.kind() == io::ErrorKind::InvalidData),
                "{err:?}"
            );
            for split in 0..=data.len() {
                assert_scan_rejects_like_decode(&data, split);
            }
        }
    }

    #[test]
    fn low_cardinality_scan_walks_linear_bytes_across_small_feeds() {
        let keys: Vec<String> = (0..20_000).map(|i| format!("user_{i}")).collect();
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let indexes: Vec<u64> = (0..20_000).collect();
        let mut lc = LC_KEY_VERSION.to_vec();
        lc.extend(lc_body(1, keys.len(), &string_values(&key_refs), &indexes));

        let rows = 20_000;
        let mut map = LC_KEY_VERSION.to_vec();
        map.extend(offsets(
            &(1..=rows as u64).map(|i| 2 * i).collect::<Vec<_>>(),
        ));
        map.extend(lc_body(
            0,
            3,
            &string_values(&["", "k1", "k2"]),
            &(0..2 * rows as u64).map(|i| 1 + i % 2).collect::<Vec<_>>(),
        ));
        let value = "v".repeat(40);
        map.extend(string_values(&vec![value.as_str(); 2 * rows]));

        for (num_rows, type_name, column) in [
            (rows, "LowCardinality(String)", lc),
            (rows, "Map(LowCardinality(String), String)", map),
        ] {
            let data = make_block_at(54460, num_rows, &[("c", type_name, column)]);
            let mut scanner = BlockScanner::new(options_at(54460));
            for end in (1024..data.len()).step_by(1024) {
                assert_eq!(scanner.scan(&data[..end]).unwrap(), BlockBoundary::NeedMore);
            }
            assert!(matches!(
                scanner.scan(&data).unwrap(),
                BlockBoundary::Complete { consumed, .. } if consumed == data.len()
            ));
            assert!(
                scanner.progress.bytes_walked < 2 * data.len() as u64,
                "{type_name}: scanner walked {} bytes for a {}-byte block",
                scanner.progress.bytes_walked,
                data.len()
            );
        }
    }

    /// Differential check against every real-server Native fixture: each block
    /// at every two-way split (and bytewise through one scanner) must report
    /// exactly the boundary and shape `decode_next_block` produces.
    #[test]
    fn real_server_fixtures_scan_matches_decode_at_every_split() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // `_lz4.native` holds compressed frames, not a bare Native stream.
            if !name.ends_with(".native") || name.ends_with("_lz4.native") {
                continue;
            }
            let revision = if name.contains("rev54485") {
                54485
            } else if name.contains("rev54460") {
                54460
            } else {
                assert!(name.contains("rev0"), "fixture {name} names no revision");
                0
            };
            let data = std::fs::read(&path).unwrap();
            let mut start = 0;
            while start < data.len() {
                let rest = &data[start..];
                assert_scan_matches_decode(rest, revision);
                let BlockBoundary::Complete { consumed, .. } = decoded_boundary(rest, revision)
                else {
                    unreachable!()
                };
                start += consumed;
            }
            assert_stream_scan_matches_decode(&data, revision, 61);
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} fixtures found");
    }
}
