use super::*;
use crate::bitmap::Bitmap;
use crate::column::{DecimalColumn, DictionaryColumn, PrimitiveColumn};
use crate::native::decode::{decode_all_bytes, DecodeOptions, DBMS_TCP_PROTOCOL_VERSION};
use crate::native::encode::validate::type_depth;
use crate::native::protocol::MAX_TYPE_DEPTH;
use crate::native::type_parser::parse_ch_type;
use crate::schema::{GeoKind, IntervalKind, Schema};

mod bool;
mod containers;
mod decimal;
mod interval;
mod low_cardinality;
mod nullable;
mod numeric;
mod saf_geo;
mod special;
mod string;
mod temporal;
mod validation;
mod wide_int;

/// Build a `Utf8Column` from raw byte values, computing the Arrow offsets the
/// same way the decoder does (starting at 0, one entry past each value).
fn utf8_column(values: &[&[u8]]) -> Utf8Column {
    let mut offsets = Vec::with_capacity(values.len() + 1);
    let mut data = Vec::new();
    offsets.push(0i32);
    for v in values {
        data.extend_from_slice(v);
        offsets.push(data.len() as i32);
    }
    Utf8Column::new(offsets, data)
}

/// Build a `FixedBinaryColumn` of the given width from equal-width byte
/// values, concatenated into the contiguous data buffer.
fn fixed_binary_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(
            v.len(),
            width,
            "fixed-string test value must be {width} bytes"
        );
        data.extend_from_slice(v);
    }
    FixedBinaryColumn::new(data, width)
}

/// Build a DecimalColumn of the given width from equal-width raw wire-order
/// byte values.
fn decimal_column(width: usize, precision: u8, scale: u8, values: &[&[u8]]) -> DecimalColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(v.len(), width, "decimal test value must be {width} bytes");
        data.extend_from_slice(v);
    }
    DecimalColumn::new(data, width, precision, scale)
}

/// Build a wide-int `FixedBinaryColumn` of the given width (16 or 32) from
/// equal-width raw wire-order byte values. Physically a FixedBinaryColumn, so
/// this delegates to `fixed_binary_column`.
fn wide_int_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    fixed_binary_column(width, values)
}

/// Build a `MapColumn` from Arrow-shaped offsets plus the keys and values
/// columns.
fn map_column(offsets: Vec<i64>, keys: Column, values: Column) -> MapColumn {
    let total = keys.len();
    MapColumn::new(
        offsets,
        Column::Tuple(TupleColumn::new(vec![keys, values], total)),
    )
}

/// Compare two columns for the types this encoder covers. Used recursively
/// for `LowCardinality` dictionary values.
fn assert_columns_eq(left: &Column, right: &Column, label: &str) {
    macro_rules! eq {
        ($va:expr, $vb:expr) => {
            assert_eq!($va.values, $vb.values, "{label} values differ")
        };
    }
    match (left, right) {
        (Column::Bool(x), Column::Bool(y)) => {
            assert_eq!(x.len, y.len, "{label} bool len differs");
            for row in 0..x.len {
                assert_eq!(x.get(row), y.get(row), "{label} bool row {row} differs");
            }
        }
        (Column::Int8(x), Column::Int8(y)) => eq!(x, y),
        (Column::Int16(x), Column::Int16(y)) => eq!(x, y),
        (Column::Int32(x), Column::Int32(y)) => eq!(x, y),
        (Column::Int64(x), Column::Int64(y)) => eq!(x, y),
        (Column::UInt8(x), Column::UInt8(y)) => eq!(x, y),
        (Column::UInt16(x), Column::UInt16(y)) => eq!(x, y),
        (Column::UInt32(x), Column::UInt32(y)) => eq!(x, y),
        (Column::UInt64(x), Column::UInt64(y)) => eq!(x, y),
        (Column::Float32(x), Column::Float32(y)) => eq!(x, y),
        (Column::Float64(x), Column::Float64(y)) => eq!(x, y),
        (Column::Date(x), Column::Date(y)) => eq!(x, y),
        (Column::Date32(x), Column::Date32(y)) => eq!(x, y),
        (Column::DateTime(x), Column::DateTime(y)) => eq!(x, y),
        (Column::DateTime64(x), Column::DateTime64(y)) => eq!(x, y),
        (Column::Time(x), Column::Time(y)) => eq!(x, y),
        (Column::Time64(x), Column::Time64(y)) => eq!(x, y),
        (Column::Interval(x), Column::Interval(y)) => eq!(x, y),
        (Column::Enum8(x), Column::Enum8(y)) => eq!(x, y),
        (Column::Enum16(x), Column::Enum16(y)) => eq!(x, y),
        (Column::Ipv4(x), Column::Ipv4(y)) => eq!(x, y),
        (Column::Uuid(x), Column::Uuid(y))
        | (Column::Ipv6(x), Column::Ipv6(y))
        | (Column::Int128(x), Column::Int128(y))
        | (Column::UInt128(x), Column::UInt128(y))
        | (Column::Int256(x), Column::Int256(y))
        | (Column::UInt256(x), Column::UInt256(y)) => {
            assert_eq!(x.width, y.width, "{label} width differ");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::Utf8(x), Column::Utf8(y)) => {
            assert_eq!(x.offsets, y.offsets, "{label} offsets differ");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::FixedBinary(x), Column::FixedBinary(y)) => {
            assert_eq!(x.width, y.width, "{label} width differ");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::Decimal(x), Column::Decimal(y)) => {
            assert_eq!(x.width, y.width, "{label} width differ");
            assert_eq!(x.precision, y.precision, "{label} precision differs");
            assert_eq!(x.scale, y.scale, "{label} scale differs");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::Dictionary(x), Column::Dictionary(y)) => {
            assert_eq!(x.indices, y.indices, "{label} dictionary indices differ");
            let dict_label = format!("{label} dictionary");
            assert_columns_eq(x.values.as_ref(), y.values.as_ref(), &dict_label);
        }
        (Column::Array(x), Column::Array(y)) => {
            assert_eq!(x.offsets, y.offsets, "{label} array offsets differ");
            let elem_label = format!("{label} array elements");
            assert_columns_eq(x.values.as_ref(), y.values.as_ref(), &elem_label);
        }
        (Column::Tuple(x), Column::Tuple(y)) => {
            assert_eq!(x.len, y.len, "{label} tuple len differs");
            assert_eq!(
                x.fields.len(),
                y.fields.len(),
                "{label} tuple field count differs"
            );
            for (i, (a, b)) in x.fields.iter().zip(&y.fields).enumerate() {
                assert_columns_eq(a, b, &format!("{label} tuple element {i}"));
            }
        }
        (Column::Map(x), Column::Map(y)) => {
            assert_eq!(x.offsets, y.offsets, "{label} map offsets differ");
            let entries_label = format!("{label} map entries");
            assert_columns_eq(x.entries.as_ref(), y.entries.as_ref(), &entries_label);
        }
        (other_a, other_b) => panic!("{label}: unexpected {other_a:?} vs {other_b:?}"),
    }

    // Validity (the null map or dictionary-index validity) must survive the
    // round-trip too. Both sides must agree on presence and on every row's
    // valid/null bit.
    match (left.validity(), right.validity()) {
        (None, None) => {}
        (Some(x), Some(y)) => {
            assert_eq!(x.len(), y.len(), "{label} validity len differs");
            for row in 0..x.len() {
                assert_eq!(
                    x.is_valid(row),
                    y.is_valid(row),
                    "{label} validity row {row} differs"
                );
            }
        }
        (x, y) => panic!(
            "{label} validity presence differs: {} vs {}",
            x.is_some(),
            y.is_some()
        ),
    }
}

/// Compare two batches column by column. Panics on any unexpected variant so
/// a wrong decode is loud.
fn assert_batches_eq(left: &ColBatch, right: &ColBatch) {
    assert_eq!(left.schema, right.schema, "schema mismatch");
    assert_eq!(left.num_rows, right.num_rows, "row count mismatch");
    assert_eq!(
        left.columns.len(),
        right.columns.len(),
        "column count mismatch"
    );
    for (i, (a, b)) in left.columns.iter().zip(&right.columns).enumerate() {
        assert_columns_eq(a, b, &format!("column {i}"));
    }
}

/// Encode `batch` as one block at `revision`, decode it back, and assert the
/// buffers survived unchanged.
fn roundtrip(batch: &ColBatch, revision: u64) {
    let bytes = encode_block(
        batch,
        &EncodeOptions {
            protocol_revision: revision,
        },
    )
    .unwrap();
    let decoded = decode_all_bytes(
        &bytes,
        &DecodeOptions {
            protocol_revision: revision,
        },
    )
    .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
    assert_eq!(decoded.num_chunks(), 1);
    assert_batches_eq(batch, &decoded.chunks[0]);
}
