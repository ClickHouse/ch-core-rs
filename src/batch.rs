use std::sync::Arc;

use crate::column::Column;
use crate::schema::Schema;

/// A batch of columnar data with schema.
#[derive(Debug, Clone)]
pub struct ColBatch {
    pub schema: Schema,
    pub columns: Vec<Column>,
    pub num_rows: usize,
}

/// A query result as a sequence of per-block chunks, NOT concatenated.
///
/// ClickHouse Native streams data as independent blocks. We keep each block
/// as its own `ColBatch` chunk rather than merging them into one buffer.
/// This avoids O(n) copies (and the O(n^2) bitmap re-packing that merging
/// bit-packed Bool/validity columns would require) and maps directly onto
/// Arrow's chunked / record-batch-stream model.
///
/// `schema` is taken from the first decoded block; all blocks of a query
/// share the same schema. Empty (zero-row) blocks are dropped from `chunks`
/// but still contribute the schema, so a zero-row result has a valid schema
/// with no chunks.
#[derive(Debug, Clone)]
pub struct ChunkedBatch {
    pub schema: Schema,
    pub chunks: Vec<Arc<ColBatch>>,
}

impl ChunkedBatch {
    pub fn num_rows(&self) -> usize {
        self.chunks.iter().map(|c| c.num_rows).sum()
    }

    pub fn num_columns(&self) -> usize {
        self.schema.num_fields()
    }

    pub fn num_chunks(&self) -> usize {
        self.chunks.len()
    }
}

impl ColBatch {
    pub fn new(schema: Schema, columns: Vec<Column>, num_rows: usize) -> Self {
        debug_assert_eq!(schema.num_fields(), columns.len());
        debug_assert!(columns.iter().all(|c| c.len() == num_rows));
        Self { schema, columns, num_rows }
    }

    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    pub fn column(&self, index: usize) -> &Column {
        &self.columns[index]
    }

    pub fn column_names(&self) -> Vec<&str> {
        self.schema.fields.iter().map(|f| f.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::PrimitiveColumn;
    use crate::schema::{ChType, Field};

    #[test]
    fn test_batch_construction() {
        let schema = Schema::new(vec![
            Field { name: "a".into(), ch_type: ChType::Int32 },
            Field { name: "b".into(), ch_type: ChType::Float64 },
        ]);
        let columns = vec![
            Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
            Column::Float64(PrimitiveColumn::new(vec![1.0, 2.0, 3.0])),
        ];
        let batch = ColBatch::new(schema, columns, 3);
        assert_eq!(batch.num_rows, 3);
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.column_names(), vec!["a", "b"]);
    }
}
