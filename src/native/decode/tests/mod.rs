use super::*;
use crate::native::varint::write_varint;

mod containers;
mod decimal;
mod framing;
mod low_cardinality;
mod numeric;
mod parser;
mod saf_geo;
mod special;
mod string;
mod temporal;

struct BlockBuilder {
    buf: Vec<u8>,
    revision: u64,
}

impl BlockBuilder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            revision: 0,
        }
    }

    /// Frame the block for a protocol revision. A revision > 0 makes
    /// `header` emit a BlockInfo preamble; a revision >= 54454 makes
    /// `column_header` emit the default (0x00) custom-serialization byte.
    fn revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }

    fn header(mut self, num_cols: usize, num_rows: usize) -> Self {
        if self.revision > 0 {
            Self::push_block_info(&mut self.buf, self.revision);
        }
        write_varint(&mut self.buf, num_cols as u64);
        write_varint(&mut self.buf, num_rows as u64);
        self
    }

    fn column_header(mut self, name: &str, type_name: &str) -> Self {
        Self::push_string(&mut self.buf, name);
        Self::push_string(&mut self.buf, type_name);
        if self.revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION {
            self.buf.push(0x00); // default serialization
        }
        self
    }

    /// Column header with an explicit custom-serialization marker (and any
    /// trailing kind bytes), regardless of revision. For framing tests.
    fn column_header_with_custom(
        mut self,
        name: &str,
        type_name: &str,
        marker: u8,
        kind_bytes: &[u8],
    ) -> Self {
        Self::push_string(&mut self.buf, name);
        Self::push_string(&mut self.buf, type_name);
        self.buf.push(marker);
        self.buf.extend_from_slice(kind_bytes);
        self
    }

    fn push_string(buf: &mut Vec<u8>, s: &str) {
        write_varint(buf, s.len() as u64);
        buf.extend_from_slice(s.as_bytes());
    }

    /// Standard BlockInfo: is_overflows=false, bucket_num=-1, and an empty
    /// out_of_order_buckets vector at revision >= 54480.
    fn push_block_info(buf: &mut Vec<u8>, revision: u64) {
        write_varint(buf, 1);
        buf.push(0x00); // is_overflows = false
        write_varint(buf, 2);
        buf.extend_from_slice(&(-1i32).to_le_bytes()); // bucket_num = -1
        if revision >= DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS {
            write_varint(buf, 3);
            write_varint(buf, 0); // empty out_of_order_buckets
        }
        write_varint(buf, 0); // terminator
    }

    fn raw_bytes(mut self, bytes: &[u8]) -> Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    fn int64_data(mut self, values: &[i64]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn int32_data(mut self, values: &[i32]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn int16_data(mut self, values: &[i16]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn int8_data(mut self, values: &[i8]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn uint64_data(mut self, values: &[u64]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn uint32_data(mut self, values: &[u32]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn date_data(mut self, values: &[u16]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn float32_data(mut self, values: &[f32]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn float64_data(mut self, values: &[f64]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn string_data(mut self, values: &[&str]) -> Self {
        for &s in values {
            write_varint(&mut self.buf, s.len() as u64);
            self.buf.extend_from_slice(s.as_bytes());
        }
        self
    }

    /// IPv4 column body: raw 4-byte LE UInt32 per row, exactly the UInt32
    /// body, so it shares `uint32_data`'s shape.
    fn ipv4_data(self, values: &[u32]) -> Self {
        self.uint32_data(values)
    }

    /// UUID / IPv6 column body: raw 16-byte rows, no length prefix, exactly
    /// a FixedString(16) body. Each entry must be 16 bytes.
    fn fixed16_data(mut self, values: &[[u8; 16]]) -> Self {
        for v in values {
            self.buf.extend_from_slice(v);
        }
        self
    }

    /// Decimal column body: raw fixed-width little-endian two's-complement
    /// integers, `width` bytes per row, no per-row framing. Each entry must
    /// already be exactly `width` bytes wide.
    fn decimal_data(mut self, rows: &[&[u8]], width: usize) -> Self {
        for r in rows {
            assert_eq!(r.len(), width, "decimal row must be {width} bytes");
            self.buf.extend_from_slice(r);
        }
        self
    }

    /// Wide-integer column body: raw contiguous little-endian fixed-width
    /// integers, `width` bytes per row (16 for Int128/UInt128, 32 for
    /// Int256/UInt256), no per-row framing. Byte-identical to a
    /// Decimal128/256 body, so it shares `decimal_data`'s shape; kept as its
    /// own name for test readability. Each entry must be exactly `width`
    /// bytes.
    fn wide_int_data(self, rows: &[&[u8]], width: usize) -> Self {
        self.decimal_data(rows, width)
    }

    fn null_map(mut self, nulls: &[bool]) -> Self {
        for &is_null in nulls {
            self.buf.push(if is_null { 0x01 } else { 0x00 });
        }
        self
    }

    /// `Array(T)` offsets: exactly `num_rows` raw little-endian u64 cumulative
    /// absolute end-offsets, with NO leading zero and no count, exactly what
    /// `SerializationArray` writes ahead of the flattened element body. The
    /// element body is appended afterward with the ordinary typed helpers
    /// (`int32_data`, `string_data`, `null_map`, `low_cardinality_*`, or a
    /// further `array_offsets` for a nested `Array`).
    fn array_offsets(mut self, offsets: &[u64]) -> Self {
        for &o in offsets {
            self.buf.extend_from_slice(&o.to_le_bytes());
        }
        self
    }

    /// Append a full `LowCardinality(T)` column block payload around an
    /// already-serialized dictionary body: the per-column key-version prefix,
    /// the index type word with the chosen index width, the dictionary entry
    /// count and `dict_bytes`, the row count, and the raw index array.
    /// `index_width` is 1/2/4/8 bytes (UInt8..UInt64); indices are written
    /// little-endian at that width. The typed `low_cardinality_*` helpers
    /// build `dict_bytes` for a given inner type and call this.
    fn low_cardinality_block(
        mut self,
        num_keys: usize,
        dict_bytes: &[u8],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        // Per-column state prefix: key version = 1.
        self.buf.extend_from_slice(&1u64.to_le_bytes());

        // Index type word: width tag in the low bits, HasAdditionalKeysBit set.
        let width_tag: u64 = match index_width {
            1 => 0,
            2 => 1,
            4 => 2,
            8 => 3,
            other => panic!("unsupported test index width {other}"),
        };
        let index_word = width_tag | LC_HAS_ADDITIONAL_KEYS_BIT;
        self.buf.extend_from_slice(&index_word.to_le_bytes());

        // Per-block dictionary: entry count then the inner-type body bytes.
        self.buf.extend_from_slice(&(num_keys as u64).to_le_bytes());
        self.buf.extend_from_slice(dict_bytes);

        // Row count, then the raw index array at the chosen width.
        self.buf
            .extend_from_slice(&(indices.len() as u64).to_le_bytes());
        for &idx in indices {
            match index_width {
                1 => self.buf.push(idx as u8),
                2 => self.buf.extend_from_slice(&(idx as u16).to_le_bytes()),
                4 => self.buf.extend_from_slice(&(idx as u32).to_le_bytes()),
                8 => self.buf.extend_from_slice(&idx.to_le_bytes()),
                _ => unreachable!(),
            }
        }
        self
    }

    /// `LowCardinality(String)` block: dictionary entries are varint len +
    /// raw bytes, exactly a plain `String` column body.
    fn low_cardinality_string(
        self,
        dictionary: &[&str],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        let mut dict_bytes = Vec::new();
        for &s in dictionary {
            write_varint(&mut dict_bytes, s.len() as u64);
            dict_bytes.extend_from_slice(s.as_bytes());
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(UInt32)` block: dictionary entries are raw 4-byte LE
    /// primitives, exactly a plain `UInt32` column body. The `DateTime` and
    /// other 4-byte numeric inners share this body shape.
    fn low_cardinality_u32(self, dictionary: &[u32], indices: &[u64], index_width: usize) -> Self {
        let mut dict_bytes = Vec::new();
        for &v in dictionary {
            dict_bytes.extend_from_slice(&v.to_le_bytes());
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(Date)` block: dictionary entries are raw 2-byte LE
    /// `UInt16` days, exactly a plain `Date`/`UInt16` column body.
    fn low_cardinality_u16(self, dictionary: &[u16], indices: &[u64], index_width: usize) -> Self {
        let mut dict_bytes = Vec::new();
        for &v in dictionary {
            dict_bytes.extend_from_slice(&v.to_le_bytes());
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(FixedString(N))` block: dictionary entries are raw
    /// fixed-width bytes, exactly a plain `FixedString(N)` column body. Each
    /// entry must already be `N` bytes wide.
    fn low_cardinality_fixed(
        self,
        dictionary: &[&[u8]],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        let mut dict_bytes = Vec::new();
        for &entry in dictionary {
            dict_bytes.extend_from_slice(entry);
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(UUID)` / `LowCardinality(IPv6)` block: dictionary
    /// entries are raw 16-byte rows, exactly a plain UUID/IPv6 column body.
    fn low_cardinality_fixed16(
        self,
        dictionary: &[[u8; 16]],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        let mut dict_bytes = Vec::new();
        for entry in dictionary {
            dict_bytes.extend_from_slice(entry);
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    fn build(self) -> Vec<u8> {
        self.buf
    }
}

/// Resolve a dictionary column's row `i` to its dictionary value bytes,
/// treating a null index as `None`. Used by the LowCardinality tests to
/// assert the observable per-row values the way a consumer would read them.
fn lc_value(col: &Column, row: usize) -> Option<Vec<u8>> {
    match col {
        Column::Dictionary(d) => {
            if let Some(bm) = &d.validity {
                if !bm.is_valid(row) {
                    return None;
                }
            }
            let idx = d.indices[row] as usize;
            match d.values.as_ref() {
                Column::Utf8(v) => Some(v.value(idx).to_vec()),
                other => panic!("expected Utf8 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Resolve a `UInt32`-valued dictionary column's row `i` to its value,
/// treating a null index as `None`, the way a consumer reads it.
fn lc_u32_value(col: &Column, row: usize) -> Option<u32> {
    match col {
        Column::Dictionary(d) => {
            if d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row)) {
                return None;
            }
            let idx = d.indices[row] as usize;
            match d.values.as_ref() {
                Column::UInt32(v) => Some(v.values[idx]),
                other => panic!("expected UInt32 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Resolve a `Date` (UInt16) dictionary column's row `i` to its value.
fn lc_date_value(col: &Column, row: usize) -> Option<u16> {
    match col {
        Column::Dictionary(d) => {
            if d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row)) {
                return None;
            }
            let idx = d.indices[row] as usize;
            match d.values.as_ref() {
                Column::Date(v) => Some(v.values[idx]),
                other => panic!("expected Date dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// The 16 wire bytes for RFC UUID `00112233-4455-6677-8899-aabbccddeeff`.
///
/// ClickHouse `SerializationUUID` dumps the UInt128 POD (items[0] then
/// items[1], each little-endian on LE servers), which is NOT RFC-4122 byte
/// order. The wire->RFC mapping a binding applies is `rfc[i] = wire[7-i]` for
/// i in 0..7 and `rfc[i] = wire[23-i]` for i in 8..15 (reverse the first 8
/// bytes, reverse the last 8). The decoder itself does no reordering; these
/// are the bytes the server emits and the bytes the decoder must return.
/// Confirmed against the live server at v26.6.1.1193-stable.
const UUID_00112233_WIRE: [u8; 16] = [
    0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88,
];

/// A 16-byte little-endian buffer with `b[0] = low`, the rest zero.
fn w16(low: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = low;
    b
}

/// A 32-byte little-endian buffer with `b[0] = low`, the rest zero.
fn w32(low: u8) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[0] = low;
    b
}

/// Borrow the inner `ArrayColumn` of a decoded `Array` column, panicking with
/// a useful message on any other variant. Keeps the assertions below terse.
fn as_array(col: &Column) -> &ArrayColumn {
    match col {
        Column::Array(a) => a,
        other => panic!("expected Array, got {other:?}"),
    }
}

/// Borrow the inner `TupleColumn` of a decoded `Tuple` column.
fn as_tuple(column: &Column) -> &crate::column::TupleColumn {
    match column {
        Column::Tuple(c) => c,
        other => panic!("expected Tuple column, got {other:?}"),
    }
}

/// Borrow the inner `MapColumn` of a decoded `Map` column.
fn as_map(column: &Column) -> &crate::column::MapColumn {
    match column {
        Column::Map(c) => c,
        other => panic!("expected Map column, got {other:?}"),
    }
}

/// Borrow a `MapColumn`'s keys and values columns out of its two-field
/// entries tuple.
fn map_entries(map: &crate::column::MapColumn) -> (&Column, &Column) {
    let t = as_tuple(map.entries.as_ref());
    assert_eq!(t.fields.len(), 2, "entries must be the (keys, values) pair");
    (&t.fields[0], &t.fields[1])
}
