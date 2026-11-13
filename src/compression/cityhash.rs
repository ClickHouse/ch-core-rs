//! CityHash128 v1.0.2 checksums used by ClickHouse compression frames.
//!
//! ClickHouse froze the algorithm at v1.0.2 because later CityHash releases
//! changed persisted hash values. `cityhash-rs` exposes that version explicitly
//! as `cityhash_102_128`.

/// CityHash 128-bit result in ClickHouse wire field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct U128 {
    pub(super) low64: u64,
    pub(super) high64: u64,
}

/// Compute CityHash128 v1.0.2 and return ClickHouse's low64/high64 fields.
pub(super) fn city_hash128(input: &[u8]) -> U128 {
    let hash = cityhash_rs::cityhash_102_128(input);
    // cityhash-rs packs the first CityHash uint64 into the numeric u128's high
    // half. ClickHouse names that field low64 and writes it first on the wire.
    U128 {
        low64: (hash >> 64) as u64,
        high64: hash as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cityhash128_matches_server_v1_0_2_vectors() {
        // Generated from ClickHouse's vendored
        // contrib/cityhash102/src/city.cc at v26.8.1.2041-lts.
        let vectors: Vec<(Vec<u8>, u64, u64)> = vec![
            (b"".to_vec(), 0x3df0_9dfc_64c0_9a2b, 0x3cb5_40c3_92e5_1e29),
            (b"a".to_vec(), 0xd271_39a1_afe0_1ad0, 0xfd7e_8ee2_e4c8_6cf6),
            (
                b"abc".to_vec(),
                0x900f_f195_5777_48fe,
                0x13a9_1763_55b2_0d7e,
            ),
            (
                b"1234567".to_vec(),
                0x8e87_77e8_75b9_7597,
                0x8852_d137_3eb8_453b,
            ),
            (
                b"12345678".to_vec(),
                0x1d77_57a6_dd6f_d9f3,
                0x7282_a0f9_c384_7a18,
            ),
            (
                b"123456789".to_vec(),
                0xc43e_148a_b194_7224,
                0xc0d0_5d34_783e_c23a,
            ),
            (
                b"hello, world".to_vec(),
                0x07b1_912f_8e71_f73d,
                0x30e5_f73c_73df_a645,
            ),
            (
                b"0123456789abcdef".to_vec(),
                0xc52e_a1ad_b29e_4800,
                0x7369_a2fa_b076_de4c,
            ),
            (
                b"The quick brown fox jumps over the lazy dog".to_vec(),
                0x6910_2202_d326_a2fd,
                0xe4b1_346b_bee5_31a1,
            ),
            (
                (0..64u8).map(|index| b'0' + index % 10).collect(),
                0x028e_b69e_8794_a562,
                0x8077_76f0_30d4_219f,
            ),
            (
                (0..128u8).map(|index| b'A' + index % 26).collect(),
                0x292f_5e16_51d7_427e,
                0x1251_bc99_ab91_8911,
            ),
            (
                (0..144u16).map(|index| b'A' + (index % 26) as u8).collect(),
                0x1aab_fe9c_daca_31ab,
                0xc49a_6d70_ac87_c24d,
            ),
            (
                (0..200u16).map(|index| index as u8).collect(),
                0x2757_7469_9e23_b6fd,
                0x5682_ef01_ebe7_289c,
            ),
            (
                (0..300u16).map(|index| index as u8).collect(),
                0x8f4a_d51c_9a58_4560,
                0xa8f4_7697_16bb_690a,
            ),
            (
                vec![
                    0x82, 0x12, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x11, 0x12, 0x13,
                    0x14, 0x15, 0x16, 0x17, 0x18,
                ],
                0x9758_b2a0_356a_3b00,
                0x3ca4_3187_7162_c4a2,
            ),
        ];

        for (input, low64, high64) in vectors {
            assert_eq!(
                city_hash128(&input),
                U128 { low64, high64 },
                "input length {}",
                input.len()
            );
        }
    }
}
