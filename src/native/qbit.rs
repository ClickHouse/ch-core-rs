/// Transpose the 8x8 bit matrix held in a `u64`: byte `i` is row `i`, bit `j`
/// of byte `i` is column `j`. Hacker's Delight figure 7-3.
#[inline(always)]
pub(crate) fn transpose8(mut x: u64) -> u64 {
    let t = (x ^ (x >> 7)) & 0x00AA_00AA_00AA_00AA;
    x ^= t ^ (t << 7);
    let t = (x ^ (x >> 14)) & 0x0000_CCCC_0000_CCCC;
    x ^= t ^ (t << 14);
    let t = (x ^ (x >> 28)) & 0x0000_0000_F0F0_F0F0;
    x ^= t ^ (t << 28);
    x
}

/// Native-width bit pattern of one QBit scalar element.
pub(crate) trait QBitWord: Copy + Default {
    /// Byte width of the element, i.e. bit planes / 8.
    const BYTES: usize;

    fn byte(self, index: usize) -> u8;

    fn set_byte(&mut self, index: usize, byte: u8);
}

macro_rules! qbit_word {
    ($ty:ty) => {
        impl QBitWord for $ty {
            const BYTES: usize = std::mem::size_of::<$ty>();

            #[inline(always)]
            fn byte(self, index: usize) -> u8 {
                (self >> (8 * index)) as u8
            }

            #[inline(always)]
            fn set_byte(&mut self, index: usize, byte: u8) {
                *self |= <$ty>::from(byte) << (8 * index);
            }
        }
    };
}

qbit_word!(u16);
qbit_word!(u32);
qbit_word!(u64);
