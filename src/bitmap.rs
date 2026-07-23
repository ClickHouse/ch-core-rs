/// Bit-packed validity bitmap using Arrow convention: bit=1 means valid, bit=0 means null.
#[derive(Debug, Clone)]
pub struct Bitmap {
    buffer: Vec<u8>,
    len: usize,
}

impl Bitmap {
    /// Create a bitmap where all values are valid.
    pub fn all_valid(len: usize) -> Self {
        let num_bytes = len.div_ceil(8);
        Self {
            buffer: vec![0xFF; num_bytes],
            len,
        }
    }

    /// Convert from ClickHouse null map format.
    ///
    /// ClickHouse sends 1 byte per row: 0x00 = valid, 0x01 = null.
    /// Arrow uses bit-packed: 1 = valid, 0 = null.
    pub fn from_ch_null_map(null_bytes: &[u8]) -> Self {
        let len = null_bytes.len();
        let mut buffer = Vec::with_capacity(len.div_ceil(8));

        // Pack 8 wire bytes into one bitmap byte at a time. Building each output
        // byte in a register from 8 inputs (branchless `(b == 0) as u8`) avoids
        // the per-row index-divide, index-modulo, and load-or-store of a
        // byte-at-a-time loop, which the data-dependent branch also kept from
        // vectorizing. Bit order stays LSB-first, valid (0x00) -> bit 1.
        let mut chunks = null_bytes.chunks_exact(8);
        for c in &mut chunks {
            let byte = (c[0] == 0) as u8
                | (((c[1] == 0) as u8) << 1)
                | (((c[2] == 0) as u8) << 2)
                | (((c[3] == 0) as u8) << 3)
                | (((c[4] == 0) as u8) << 4)
                | (((c[5] == 0) as u8) << 5)
                | (((c[6] == 0) as u8) << 6)
                | (((c[7] == 0) as u8) << 7);
            buffer.push(byte);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut byte = 0u8;
            for (k, &b) in rem.iter().enumerate() {
                byte |= ((b == 0) as u8) << k;
            }
            buffer.push(byte);
        }

        Self { buffer, len }
    }

    /// Check if the value at `index` is valid (not null).
    pub fn is_valid(&self, index: usize) -> bool {
        assert!(index < self.len);
        (self.buffer[index / 8] >> (index % 8)) & 1 == 1
    }

    /// Count the number of null values.
    pub fn null_count(&self) -> usize {
        self.len - self.valid_count()
    }

    /// Count the number of valid values.
    pub fn valid_count(&self) -> usize {
        // Count set bits, but only up to self.len
        if self.len == 0 {
            return 0;
        }
        let full_bytes = self.len / 8;
        let remaining_bits = self.len % 8;

        let mut count: usize = self.buffer[..full_bytes]
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum();

        if remaining_bits > 0 {
            let mask = (1u8 << remaining_bits) - 1;
            count += (self.buffer[full_bytes] & mask).count_ones() as usize;
        }

        count
    }

    /// Get the raw byte buffer (Arrow layout).
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }

    /// Number of logical bits.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Create a bitmap from a pre-built byte buffer.
    pub fn from_raw(buffer: Vec<u8>, len: usize) -> Self {
        debug_assert!(len.div_ceil(8) <= buffer.len());
        Self { buffer, len }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_valid() {
        let bm = Bitmap::all_valid(10);
        assert_eq!(bm.len(), 10);
        assert_eq!(bm.null_count(), 0);
        for i in 0..10 {
            assert!(bm.is_valid(i));
        }
    }

    #[test]
    fn test_from_ch_null_map_no_nulls() {
        let null_map = vec![0x00; 5]; // all valid
        let bm = Bitmap::from_ch_null_map(&null_map);
        assert_eq!(bm.len(), 5);
        assert_eq!(bm.null_count(), 0);
        for i in 0..5 {
            assert!(bm.is_valid(i));
        }
    }

    #[test]
    fn test_from_ch_null_map_with_nulls() {
        // Rows: valid, null, valid, null, valid
        let null_map = vec![0x00, 0x01, 0x00, 0x01, 0x00];
        let bm = Bitmap::from_ch_null_map(&null_map);
        assert_eq!(bm.len(), 5);
        assert_eq!(bm.null_count(), 2);
        assert!(bm.is_valid(0));
        assert!(!bm.is_valid(1));
        assert!(bm.is_valid(2));
        assert!(!bm.is_valid(3));
        assert!(bm.is_valid(4));
    }

    #[test]
    fn test_from_ch_null_map_all_null() {
        let null_map = vec![0x01; 8];
        let bm = Bitmap::from_ch_null_map(&null_map);
        assert_eq!(bm.null_count(), 8);
        for i in 0..8 {
            assert!(!bm.is_valid(i));
        }
    }

    #[test]
    fn test_empty_bitmap() {
        let bm = Bitmap::from_ch_null_map(&[]);
        assert_eq!(bm.len(), 0);
        assert!(bm.is_empty());
        assert_eq!(bm.null_count(), 0);
    }
}
