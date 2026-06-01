/// Bit-packed validity bitmap using Arrow convention: bit=1 means valid, bit=0 means null.
#[derive(Debug, Clone)]
pub struct Bitmap {
    buffer: Vec<u8>,
    len: usize,
}

impl Bitmap {
    /// Create a bitmap where all values are valid.
    pub fn all_valid(len: usize) -> Self {
        let num_bytes = (len + 7) / 8;
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
        let num_bytes = (len + 7) / 8;
        let mut buffer = vec![0u8; num_bytes];

        for (i, &b) in null_bytes.iter().enumerate() {
            if b == 0x00 {
                // valid → set bit to 1
                buffer[i / 8] |= 1 << (i % 8);
            }
            // null (b != 0) → bit stays 0
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
        debug_assert!((len + 7) / 8 <= buffer.len());
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
