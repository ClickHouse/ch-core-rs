use super::*;
use crate::batch::ColBatch;
use crate::bitmap::Bitmap;
use crate::column::{
    ArrayColumn, Column, DictionaryColumn, PrimitiveColumn, TupleColumn, Utf8Column,
};
use crate::schema::{ChType, Field, GeoKind, Schema};
use std::ffi::CStr;

mod containers;
mod low_cardinality;
mod saf_geo_nested;
mod scalar;
mod stream;

fn make_test_batch() -> Arc<ColBatch> {
    let schema = Schema::new(vec![
        Field {
            name: "i".into(),
            ch_type: ChType::Int64,
        },
        Field {
            name: "f".into(),
            ch_type: ChType::Float64,
        },
        Field {
            name: "s".into(),
            ch_type: ChType::String,
        },
    ]);
    let columns = vec![
        Column::Int64(PrimitiveColumn::new(vec![10, 20])),
        Column::Float64(PrimitiveColumn::new(vec![1.5, 2.5])),
        Column::Utf8(Utf8Column::new(vec![0, 2, 5], b"abcde".to_vec())),
    ];
    Arc::new(ColBatch::new(schema, columns, 2))
}
