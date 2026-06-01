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

use std::io::{self, Cursor};

use crate::batch::ColBatch;
use crate::native::decode::{decode_next_block, DecodeError, DecodeOptions};

/// Push-based incremental decoder for ClickHouse Native blocks.
///
/// Accumulates bytes via `feed()` and attempts to decode complete blocks
/// after each feed. Partial blocks are retained in the internal buffer
/// until enough data arrives.
pub struct StreamDecoder {
    buffer: Vec<u8>,
    /// Byte offset of unconsumed data in `buffer`.
    pos: usize,
    options: DecodeOptions,
    finished: bool,
}

impl StreamDecoder {
    pub fn new(options: DecodeOptions) -> Self {
        Self {
            buffer: Vec::new(),
            pos: 0,
            options,
            finished: false,
        }
    }

    /// Push a chunk of bytes and return any complete blocks that can be
    /// decoded from the accumulated buffer.
    ///
    /// Returns an empty vec if the chunk doesn't complete any block.
    /// Partial data is retained for the next `feed()` or `finish()` call.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<ColBatch>, DecodeError> {
        if self.finished {
            return Err(DecodeError::Io(io::Error::new(
                io::ErrorKind::Other,
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

    /// Try to decode as many complete blocks as possible from the buffer.
    fn drain_blocks(&mut self) -> Result<Vec<ColBatch>, DecodeError> {
        let mut blocks = Vec::new();

        loop {
            let data = &self.buffer[self.pos..];
            if data.is_empty() {
                break;
            }

            let mut cursor = Cursor::new(data);

            match decode_next_block(&mut cursor, &self.options) {
                Ok(Some(batch)) => {
                    // Successfully decoded a block. Advance position.
                    let consumed = cursor.position() as usize;
                    self.pos += consumed;
                    blocks.push(batch);
                    // Try to decode another block from remaining data.
                }
                Ok(None) => {
                    // EOF at block boundary — no more complete blocks.
                    // This happens when remaining data is empty (already
                    // caught above) or when we're exactly at a boundary.
                    self.pos = self.buffer.len();
                    break;
                }
                Err(DecodeError::Io(ref e))
                    if e.kind() == io::ErrorKind::UnexpectedEof =>
                {
                    // Not enough data for a complete block yet.
                    // Keep the buffer as-is and wait for more data.
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
}
