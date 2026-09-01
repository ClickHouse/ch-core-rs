use super::*;

fn as_variant(column: &Column) -> &crate::column::VariantColumn {
    match column {
        Column::Variant(column) => column,
        other => panic!("expected Geometry Variant column, got {other:?}"),
    }
}

fn first_x(mut column: &Column, array_depth: usize) -> f64 {
    for _ in 0..array_depth {
        column = match column {
            Column::Array(array) => array.values.as_ref(),
            other => panic!("expected Geometry Array level, got {other:?}"),
        };
    }
    match column {
        Column::Tuple(point) => match &point.fields[0] {
            Column::Float64(x) => x.values[0],
            other => panic!("expected Point X Float64 child, got {other:?}"),
        },
        other => panic!("expected Point Tuple leaf, got {other:?}"),
    }
}

#[test]
fn test_decode_geometry_all_alternatives_and_null() {
    // Geometry is the BASIC Variant body in the server's fixed discriminator
    // order: LineString, MultiLineString, MultiPolygon, Point, Polygon, Ring,
    // MultiPoint, NULL. MultiPoint was appended at 6, preserving 0 through 5.
    // Each dense child below has one selected row.
    // Ring and MultiPoint are both Array(Point), so synthetic body bytes cannot
    // distinguish those two names. Their fixed positions are pinned by the FFI
    // child-name test and the real-server all_types integration fixture.
    let data = BlockBuilder::new()
        .header(1, 8)
        .column_header("g", "Geometry")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[0, 1, 2, 3, 4, 5, 6, u8::MAX])
        // LineString: one line with two points.
        .array_offsets(&[2])
        .float64_data(&[13.0, 14.0])
        .float64_data(&[79.0, 80.0])
        // MultiLineString: one line with two points.
        .array_offsets(&[1])
        .array_offsets(&[2])
        .float64_data(&[21.0, 22.0])
        .float64_data(&[31.0, 32.0])
        // MultiPolygon: one polygon, one ring, one point.
        .array_offsets(&[1])
        .array_offsets(&[1])
        .array_offsets(&[1])
        .float64_data(&[33.0])
        .float64_data(&[43.0])
        // Point.
        .float64_data(&[51.0])
        .float64_data(&[61.0])
        // Polygon: one ring with two points.
        .array_offsets(&[1])
        .array_offsets(&[2])
        .float64_data(&[71.0, 72.0])
        .float64_data(&[81.0, 82.0])
        // Ring: one point.
        .array_offsets(&[1])
        .float64_data(&[91.0])
        .float64_data(&[101.0])
        // MultiPoint: two points.
        .array_offsets(&[2])
        .float64_data(&[111.0, 112.0])
        .float64_data(&[121.0, 122.0])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.schema.fields[0].ch_type, ChType::Geometry);
    let geometry = as_variant(decoded.chunks[0].column(0));
    assert_eq!(geometry.len(), 8);
    assert_eq!(geometry.null_count(), 1);
    for discriminator in 0..7u8 {
        assert_eq!(
            geometry.value_position(discriminator as usize),
            Some((discriminator, 0))
        );
    }
    assert_eq!(geometry.value_position(7), Some((u8::MAX, 0)));
    assert_eq!(geometry.variants.len(), 7);
    assert_eq!(first_x(&geometry.variants[0], 1), 13.0);
    assert_eq!(first_x(&geometry.variants[1], 2), 21.0);
    assert_eq!(first_x(&geometry.variants[2], 3), 33.0);
    assert_eq!(first_x(&geometry.variants[3], 0), 51.0);
    assert_eq!(first_x(&geometry.variants[4], 2), 71.0);
    assert_eq!(first_x(&geometry.variants[5], 1), 91.0);
    assert_eq!(first_x(&geometry.variants[6], 1), 111.0);
}

#[test]
fn test_decode_geometry_zero_rows() {
    // NativeWriter's rows gate suppresses the complete Variant payload,
    // including Geometry's BASIC mode word and every child prefix.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("g", "Geometry")
        .build();
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema.fields[0].ch_type, ChType::Geometry);

    let empty = empty_column(&ChType::Geometry);
    let geometry = as_variant(&empty);
    assert!(geometry.is_empty());
    assert_eq!(geometry.variants.len(), 7);
    assert!(geometry.variants.iter().all(Column::is_empty));
}

#[test]
fn test_decode_geometry_multi_block() {
    let first = BlockBuilder::new()
        .header(1, 2)
        .column_header("g", "Geometry")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[3, u8::MAX])
        // Only the Point child is nonempty.
        .float64_data(&[13.0])
        .float64_data(&[79.0])
        .build();
    let second = BlockBuilder::new()
        .header(1, 2)
        .column_header("g", "Geometry")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[0, 6])
        // LineString child.
        .array_offsets(&[1])
        .float64_data(&[21.0])
        .float64_data(&[31.0])
        // MultiPoint child.
        .array_offsets(&[2])
        .float64_data(&[41.0, 42.0])
        .float64_data(&[51.0, 52.0])
        .build();
    let mut data = first;
    data.extend_from_slice(&second);

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    let first = as_variant(decoded.chunks[0].column(0));
    assert_eq!(first.value_position(0), Some((3, 0)));
    assert_eq!(first.value_position(1), Some((u8::MAX, 0)));
    let second = as_variant(decoded.chunks[1].column(0));
    assert_eq!(second.value_position(0), Some((0, 0)));
    assert_eq!(second.value_position(1), Some((6, 0)));
    assert_eq!(first_x(&second.variants[6], 1), 41.0);
}
