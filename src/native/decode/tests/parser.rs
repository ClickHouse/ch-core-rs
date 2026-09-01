use super::*;

#[test]
fn parse_nothing_and_nullable_nothing_are_case_sensitive() {
    assert_eq!(parse_ch_type("Nothing"), Some(ChType::Nothing));
    assert_eq!(
        parse_ch_type("Nullable(Nothing)"),
        Some(ChType::Nullable(Box::new(ChType::Nothing)))
    );
    assert_eq!(ChType::Nothing.to_string(), "Nothing");
    assert_eq!(parse_ch_type("nothing"), None);
    assert_eq!(parse_ch_type("NOTHING"), None);
    assert_eq!(parse_ch_type("Nothing "), None);
}

#[test]
fn test_nested_nullable_type_is_rejected() {
    // `Nullable(Nullable(T))` is not a type ClickHouse emits, and the
    // single-`Nullable` unwrap in decode/scan cannot handle it, so it must be
    // rejected at header parse time rather than panic on the inner wrapper.
    // Exercise both a row-bearing and a zero-row block, and both the allocating
    // decode and the completeness scan, since the review found a distinct panic
    // site on each path.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("n", "Nullable(Nullable(Int32))")
            .build();
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "decode should reject nested Nullable at {num_rows} rows"
        );
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "scan should reject nested Nullable at {num_rows} rows"
        );
    }
}

#[test]
fn test_nullable_low_cardinality_type_is_rejected() {
    // `Nullable(LowCardinality(T))` is the illegal nesting direction (only
    // `LowCardinality(Nullable(T))` is legal). The inner `LowCardinality` is
    // checked only at the top level, so accepting this shape would reach an
    // `unreachable!`; it must be rejected at header parse time instead.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("n", "Nullable(LowCardinality(String))")
            .build();
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "decode should reject Nullable(LowCardinality) at {num_rows} rows"
        );
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "scan should reject Nullable(LowCardinality) at {num_rows} rows"
        );
    }
}

#[test]
fn test_low_cardinality_nullable_still_accepted() {
    // The legal direction, `LowCardinality(Nullable(T))`, must still parse: the
    // parse-time rejection only refuses wrappers nested inside `Nullable`, not
    // this one. A zero-row block is enough to prove the type parses and is
    // decodable without needing a full dictionary body.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("lc", "LowCardinality(Nullable(String))")
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String))))
    );
    assert!(block_end(&data, &DecodeOptions::default()).is_ok());
}

#[test]
fn test_parse_ch_type_temporal() {
    assert_eq!(parse_ch_type("Date"), Some(ChType::Date));
    assert_eq!(parse_ch_type("Date32"), Some(ChType::Date32));
    assert_eq!(
        parse_ch_type("DateTime"),
        Some(ChType::DateTime { timezone: None })
    );
    assert_eq!(
        parse_ch_type("DateTime('UTC')"),
        Some(ChType::DateTime {
            timezone: Some("UTC".to_string())
        })
    );
    assert_eq!(
        parse_ch_type("DateTime64(3)"),
        Some(ChType::DateTime64 {
            precision: 3,
            timezone: None
        })
    );
    assert_eq!(
        parse_ch_type("DateTime64(3, 'UTC')"),
        Some(ChType::DateTime64 {
            precision: 3,
            timezone: Some("UTC".to_string())
        })
    );
    assert_eq!(
        parse_ch_type("DateTime64(9)"),
        Some(ChType::DateTime64 {
            precision: 9,
            timezone: None
        })
    );
    // Precision above the 0..=9 range is unsupported, surfaced as None so
    // the caller reports UnsupportedType.
    assert_eq!(parse_ch_type("DateTime64(10)"), None);
    assert_eq!(parse_ch_type("Time"), Some(ChType::Time));
    assert_eq!(
        parse_ch_type("Time64(3)"),
        Some(ChType::Time64 { precision: 3 })
    );
    assert_eq!(
        parse_ch_type("Time64(9)"),
        Some(ChType::Time64 { precision: 9 })
    );
    // Accept only canonical strings emitted in Native headers. Bare Time64
    // is an input shorthand for Time64(3), not an emitted spelling.
    for unsupported in [
        "Time()",
        "Time(3)",
        "Time('UTC')",
        "Time64",
        "Time64()",
        "Time64(03)",
        "Time64(10)",
        "Time64(3, 'UTC')",
        "Time64(3, '')",
    ] {
        assert_eq!(parse_ch_type(unsupported), None, "accepted {unsupported}");
    }
}

#[test]
fn test_parse_ch_type_intervals() {
    let cases = [
        ("IntervalYear", IntervalKind::Year),
        ("IntervalQuarter", IntervalKind::Quarter),
        ("IntervalMonth", IntervalKind::Month),
        ("IntervalWeek", IntervalKind::Week),
        ("IntervalDay", IntervalKind::Day),
        ("IntervalHour", IntervalKind::Hour),
        ("IntervalMinute", IntervalKind::Minute),
        ("IntervalSecond", IntervalKind::Second),
        ("IntervalMillisecond", IntervalKind::Millisecond),
        ("IntervalMicrosecond", IntervalKind::Microsecond),
        ("IntervalNanosecond", IntervalKind::Nanosecond),
    ];
    for (name, kind) in cases {
        let ch_type = ChType::Interval(kind);
        assert_eq!(parse_ch_type(name), Some(ch_type.clone()));
        assert_eq!(ch_type.to_string(), name);
        assert_eq!(parse_ch_type(&ch_type.to_string()), Some(ch_type));
    }

    // Native headers use only the canonical, case-sensitive full names.
    for unsupported in [
        "Interval",
        "IntervalDay()",
        "intervalDay",
        "IntervalDays",
        "Intervalsecond",
    ] {
        assert_eq!(parse_ch_type(unsupported), None, "accepted {unsupported}");
    }
}

#[test]
fn test_parse_ch_type_bfloat16() {
    assert_eq!(parse_ch_type("BFloat16"), Some(ChType::BFloat16));
    assert_eq!(ChType::BFloat16.to_string(), "BFloat16");
    for unsupported in ["bfloat16", "Bfloat16", "BFloat16()", "BFloat32"] {
        assert_eq!(parse_ch_type(unsupported), None, "accepted {unsupported}");
    }
}

#[test]
fn test_ch_type_display_round_trips_through_parser() {
    // Display renders the canonical ClickHouse type name, which is the
    // string bindings hand to users. Every representative variant must
    // parse back to the exact same ChType.
    let cases = vec![
        ChType::Bool,
        ChType::Int8,
        ChType::Int16,
        ChType::Int32,
        ChType::Int64,
        ChType::UInt8,
        ChType::UInt16,
        ChType::UInt32,
        ChType::UInt64,
        ChType::Float32,
        ChType::Float64,
        ChType::BFloat16,
        ChType::String,
        ChType::FixedString(16),
        ChType::Date,
        ChType::Date32,
        ChType::DateTime { timezone: None },
        ChType::DateTime {
            timezone: Some("UTC".to_string()),
        },
        ChType::DateTime64 {
            precision: 3,
            timezone: None,
        },
        ChType::DateTime64 {
            precision: 9,
            timezone: Some("Asia/Istanbul".to_string()),
        },
        ChType::Time,
        ChType::Time64 { precision: 0 },
        ChType::Time64 { precision: 9 },
        ChType::Nullable(Box::new(ChType::String)),
        ChType::Nullable(Box::new(ChType::DateTime64 {
            precision: 6,
            timezone: Some("UTC".to_string()),
        })),
    ];
    for t in cases {
        let rendered = t.to_string();
        assert_eq!(
            parse_ch_type(&rendered),
            Some(t.clone()),
            "Display output {rendered:?} did not parse back to {t:?}"
        );
    }
}

#[test]
fn test_parse_ch_type_low_cardinality() {
    assert_eq!(
        parse_ch_type("LowCardinality(String)"),
        Some(ChType::LowCardinality(Box::new(ChType::String)))
    );
    assert_eq!(
        parse_ch_type("LowCardinality(Nullable(String))"),
        Some(ChType::LowCardinality(Box::new(ChType::Nullable(
            Box::new(ChType::String)
        ))))
    );
}

#[test]
fn test_ch_type_display_round_trips_low_cardinality() {
    for t in [
        ChType::LowCardinality(Box::new(ChType::String)),
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
    ] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
}

#[test]
fn test_parse_ch_type_low_cardinality_non_string() {
    // The parser records any inner type; legality is enforced at decode.
    assert_eq!(
        parse_ch_type("LowCardinality(UInt32)"),
        Some(ChType::LowCardinality(Box::new(ChType::UInt32)))
    );
    assert_eq!(
        parse_ch_type("LowCardinality(Nullable(Date))"),
        Some(ChType::LowCardinality(Box::new(ChType::Nullable(
            Box::new(ChType::Date)
        ))))
    );
    // Display round-trips back through the parser.
    for t in [
        ChType::LowCardinality(Box::new(ChType::UInt32)),
        ChType::LowCardinality(Box::new(ChType::FixedString(4))),
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::Date)))),
    ] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
}

#[test]
fn test_parse_ch_type_uuid_ipv4_ipv6() {
    assert_eq!(parse_ch_type("UUID"), Some(ChType::Uuid));
    assert_eq!(parse_ch_type("IPv4"), Some(ChType::Ipv4));
    assert_eq!(parse_ch_type("IPv6"), Some(ChType::Ipv6));
}

#[test]
fn test_ch_type_display_round_trips_uuid_ipv4_ipv6() {
    for t in [ChType::Uuid, ChType::Ipv4, ChType::Ipv6] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
    assert_eq!(ChType::Uuid.to_string(), "UUID");
    assert_eq!(ChType::Ipv4.to_string(), "IPv4");
    assert_eq!(ChType::Ipv6.to_string(), "IPv6");
}

#[test]
fn test_parse_ch_type_enum8() {
    // Enum8 maps names to Int8 values; the parser preserves order and
    // accepts negatives. Values must fit i8.
    assert_eq!(
        parse_ch_type("Enum8('pending' = 1, 'active' = 2, 'closed' = -1)"),
        Some(ChType::Enum8 {
            variants: vec![
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
                ("closed".to_string(), -1),
            ],
        })
    );
    // i8 range edges decode; one past the edge is rejected.
    assert_eq!(
        parse_ch_type("Enum8('lo' = -128, 'hi' = 127)"),
        Some(ChType::Enum8 {
            variants: vec![("lo".to_string(), -128), ("hi".to_string(), 127)],
        })
    );
    assert_eq!(parse_ch_type("Enum8('over' = 128)"), None);
    assert_eq!(parse_ch_type("Enum8('under' = -129)"), None);
}

#[test]
fn test_parse_ch_type_enum16() {
    // Enum16 maps names to Int16 values; same parser, wider range.
    assert_eq!(
        parse_ch_type("Enum16('pending' = 1, 'active' = 2, 'closed' = -1)"),
        Some(ChType::Enum16 {
            variants: vec![
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
                ("closed".to_string(), -1),
            ],
        })
    );
    assert_eq!(
        parse_ch_type("Enum16('lo' = -32768, 'hi' = 32767)"),
        Some(ChType::Enum16 {
            variants: vec![("lo".to_string(), -32768), ("hi".to_string(), 32767)],
        })
    );
    assert_eq!(parse_ch_type("Enum16('over' = 32768)"), None);
}

#[test]
fn test_parse_ch_type_enum_malformed_rejected() {
    // Each of these is a syntax error on the untrusted type string and must
    // surface as None (UnsupportedType), never a panic.
    for bad in [
        "Enum8('pending' 1)",       // missing '='
        "Enum8(pending = 1)",       // name not quoted
        "Enum8('pending' = )",      // missing value
        "Enum8('pending' = abc)",   // non-numeric value
        "Enum8('unterminated = 1)", // name never closed
        "Enum8('bad\\x' = 1)",      // unknown escape
        "Enum8('a' = 1 'b' = 2)",   // missing comma between pairs
    ] {
        assert_eq!(parse_ch_type(bad), None, "expected None for {bad:?}");
    }
}

#[test]
fn test_enum_type_string_round_trips_escaping_and_order() {
    // Display is the inverse of the parser, so parse(display(t)) == t for the
    // tricky cases: a name containing a comma and an equals sign (both pass
    // through unescaped on the wire), a name with an escaped quote and a
    // backslash, negative values, and ascending multi-value ordering.
    let cases = vec![
        ChType::Enum8 {
            variants: vec![
                ("closed".to_string(), -1),
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
            ],
        },
        // A name with a comma and an equals sign: the parser cannot split on
        // those, it walks the quotes. Display escapes neither.
        ChType::Enum8 {
            variants: vec![("a,b=c".to_string(), 7)],
        },
        // A name with an escaped quote and a backslash.
        ChType::Enum16 {
            variants: vec![
                ("x'y".to_string(), -3),
                ("back\\slash".to_string(), 4),
                ("tab\tnl\n".to_string(), 9),
            ],
        },
    ];
    for t in cases {
        let rendered = t.to_string();
        assert_eq!(
            parse_ch_type(&rendered),
            Some(t.clone()),
            "Display output {rendered:?} did not parse back to {t:?}"
        );
    }
    // Pin the exact rendering of the comma/equals case so a regression in the
    // escaping (e.g. accidentally escaping `,` or `=`) is caught.
    assert_eq!(
        ChType::Enum8 {
            variants: vec![("a,b=c".to_string(), 7)],
        }
        .to_string(),
        "Enum8('a,b=c' = 7)"
    );
    // And the escaped quote / backslash rendering.
    assert_eq!(
        ChType::Enum8 {
            variants: vec![("x'y".to_string(), 1), ("a\\b".to_string(), 2)],
        }
        .to_string(),
        "Enum8('x\\'y' = 1, 'a\\\\b' = 2)"
    );
}

#[test]
fn test_parse_ch_type_decimal() {
    // The server emits the canonical `Decimal(P, S)`; the parser derives the
    // bit width from P and validates 0 <= S <= P, 1 <= P <= 76.
    assert_eq!(
        parse_ch_type("Decimal(9, 4)"),
        Some(ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        })
    );
    assert_eq!(
        parse_ch_type("Decimal(18, 0)"),
        Some(ChType::Decimal {
            precision: 18,
            scale: 0,
            bits: 64,
        })
    );
    assert_eq!(
        parse_ch_type("Decimal(38, 38)"),
        Some(ChType::Decimal {
            precision: 38,
            scale: 38,
            bits: 128,
        })
    );
    assert_eq!(
        parse_ch_type("Decimal(76, 50)"),
        Some(ChType::Decimal {
            precision: 76,
            scale: 50,
            bits: 256,
        })
    );
    // Width derivation at every boundary, parsed end to end.
    for (p, bits) in [
        (1u8, 32u16),
        (9, 32),
        (10, 64),
        (18, 64),
        (19, 128),
        (38, 128),
        (39, 256),
        (76, 256),
    ] {
        assert_eq!(
            parse_ch_type(&format!("Decimal({p}, 0)")),
            Some(ChType::Decimal {
                precision: p,
                scale: 0,
                bits,
            }),
            "precision {p} should derive {bits} bits"
        );
    }
}

#[test]
fn test_parse_ch_type_decimal_invalid_rejected() {
    // Each is rejected as None (UnsupportedType), never a panic, on the
    // untrusted type string.
    for bad in [
        "Decimal(0, 0)",     // P below 1: no backing integer
        "Decimal(77, 0)",    // P above 76
        "Decimal(9, 10)",    // S > P
        "Decimal(5)",        // missing scale (server always emits both)
        "Decimal(a, 2)",     // non-numeric precision
        "Decimal(9, b)",     // non-numeric scale
        "Decimal(9, )",      // missing scale value
        "Decimal(, 2)",      // missing precision value
        "Decimal(9, 4, 32)", // extra field is not the canonical form
    ] {
        assert_eq!(parse_ch_type(bad), None, "expected None for {bad:?}");
    }
}

#[test]
fn test_decimal_type_string_round_trips() {
    // Display emits the canonical `Decimal(P, S)` the server writes, so
    // parse(display(t)) == t at every width, including S = 0 and S = P.
    for t in [
        ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        },
        ChType::Decimal {
            precision: 10,
            scale: 0,
            bits: 64,
        },
        ChType::Decimal {
            precision: 38,
            scale: 38,
            bits: 128,
        },
        ChType::Decimal {
            precision: 50,
            scale: 10,
            bits: 256,
        },
    ] {
        let rendered = t.to_string();
        assert_eq!(
            parse_ch_type(&rendered),
            Some(t.clone()),
            "Display {rendered:?} did not parse back to {t:?}"
        );
    }
}

#[test]
fn test_parse_ch_type_wide_int() {
    // The four exact, case-sensitive spellings the server emits; no
    // parameters, no aliases. Display round-trips each.
    for (name, ty) in [
        ("Int128", ChType::Int128),
        ("UInt128", ChType::UInt128),
        ("Int256", ChType::Int256),
        ("UInt256", ChType::UInt256),
    ] {
        assert_eq!(parse_ch_type(name), Some(ty.clone()));
        assert_eq!(ty.to_string(), name);
        assert_eq!(parse_ch_type(&ty.to_string()), Some(ty));
    }
    // No case-folding or alias forms are accepted.
    for bad in ["int128", "UINT128", "Int 128", "Int512", "UInt128(1)"] {
        assert_eq!(parse_ch_type(bad), None, "{bad} must not parse");
    }
}

#[test]
fn test_parse_ch_type_array() {
    assert_eq!(
        parse_ch_type("Array(Int32)"),
        Some(ChType::Array(Box::new(ChType::Int32)))
    );
    // Element may itself be Nullable, LowCardinality, or a further Array; there
    // is no inner-type restriction on the parser.
    assert_eq!(
        parse_ch_type("Array(Nullable(Int32))"),
        Some(ChType::Array(Box::new(ChType::Nullable(Box::new(
            ChType::Int32
        )))))
    );
    assert_eq!(
        parse_ch_type("Array(LowCardinality(String))"),
        Some(ChType::Array(Box::new(ChType::LowCardinality(Box::new(
            ChType::String
        )))))
    );
    assert_eq!(
        parse_ch_type("Array(Array(Int32))"),
        Some(ChType::Array(Box::new(ChType::Array(Box::new(
            ChType::Int32
        )))))
    );
}

#[test]
fn test_ch_type_display_round_trips_array() {
    for t in [
        ChType::Array(Box::new(ChType::Int32)),
        ChType::Array(Box::new(ChType::String)),
        ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int32)))),
        ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    ] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
}

#[test]
fn test_parse_ch_type_nullable_array_is_rejected() {
    // `Nullable(Array(T))` is not a constructible server type
    // (`DataTypeArray::canBeInsideNullable()` is false), so the parser rejects
    // it rather than producing a shape decode/scan cannot unwrap.
    assert_eq!(parse_ch_type("Nullable(Array(Int32))"), None);

    // The rejection must hold on both the allocating decode and the scan, at
    // zero and nonzero rows, matching the other illegal-nesting guards.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("a", "Nullable(Array(Int32))")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}

#[test]
fn test_parse_ch_type_rejects_over_deep_nesting() {
    // The type string is untrusted wire input; an unbounded nesting like
    // `Array(Array(...Array(Int32)...))` would overflow the stack (an
    // uncatchable SIGABRT) without a depth cap. Past MAX_TYPE_DEPTH the parser
    // returns None, which surfaces as UnsupportedType, never a crash.
    let n = MAX_TYPE_DEPTH + 100;
    let over_deep = format!("{}Int32{}", "Array(".repeat(n), ")".repeat(n));
    assert_eq!(parse_ch_type(&over_deep), None);

    // A zero-row block carrying that over-deep type must degrade to a clean
    // UnsupportedType error, not abort the process.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("a", &over_deep)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_parse_ch_type_tuple() {
    // Unnamed elements.
    assert_eq!(
        parse_ch_type("Tuple(Int32, String)"),
        Some(ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::String),
        ]))
    );
    // Zero elements.
    assert_eq!(parse_ch_type("Tuple()"), Some(ChType::Tuple(vec![])));
    // Named elements, bare identifiers.
    assert_eq!(
        parse_ch_type("Tuple(a Int32, user_2 Nullable(String))"),
        Some(ChType::Tuple(vec![
            (Some("a".to_string()), ChType::Int32),
            (
                Some("user_2".to_string()),
                ChType::Nullable(Box::new(ChType::String)),
            ),
        ]))
    );
    // A comma inside an element type's own parentheses must not split.
    assert_eq!(
        parse_ch_type("Tuple(Decimal(9, 4), Int8)"),
        Some(ChType::Tuple(vec![
            (
                None,
                ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            ),
            (None, ChType::Int8),
        ]))
    );
    // A comma inside an Enum element's quoted names must not split either.
    assert_eq!(
        parse_ch_type("Tuple(e Enum8('a,b' = 1), s String)"),
        Some(ChType::Tuple(vec![
            (
                Some("e".to_string()),
                ChType::Enum8 {
                    variants: vec![("a,b".to_string(), 1)],
                },
            ),
            (Some("s".to_string()), ChType::String),
        ]))
    );
    // Backtick-quoted names: spaces and commas just force quoting; a
    // backtick inside escapes as \` (the server's writeBackQuotedString
    // form) or as a doubled `` (accepted for parser leniency).
    assert_eq!(
        parse_ch_type("Tuple(`a b` Int8, `c,d` Int8, `e\\`f` Int8, `g``h` Int8)"),
        Some(ChType::Tuple(vec![
            (Some("a b".to_string()), ChType::Int8),
            (Some("c,d".to_string()), ChType::Int8),
            (Some("e`f".to_string()), ChType::Int8),
            (Some("g`h".to_string()), ChType::Int8),
        ]))
    );
    // Keyword names arrive backtick-quoted from the server.
    assert_eq!(
        parse_ch_type("Tuple(`select` Int8)"),
        Some(ChType::Tuple(vec![(
            Some("select".to_string()),
            ChType::Int8,
        )]))
    );
    // Containers compose: Tuple in Array, Array in Tuple, nested Tuple,
    // Nullable(Tuple).
    assert_eq!(
        parse_ch_type("Array(Tuple(Int32, Int32))"),
        Some(ChType::Array(Box::new(ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::Int32),
        ]))))
    );
    assert_eq!(
        parse_ch_type("Tuple(a Tuple(b Int8), c Array(String))"),
        Some(ChType::Tuple(vec![
            (
                Some("a".to_string()),
                ChType::Tuple(vec![(Some("b".to_string()), ChType::Int8)]),
            ),
            (
                Some("c".to_string()),
                ChType::Array(Box::new(ChType::String)),
            ),
        ]))
    );
    assert_eq!(
        parse_ch_type("Nullable(Tuple(Int32, String))"),
        Some(ChType::Nullable(Box::new(ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::String),
        ]))))
    );

    // Malformed inputs are rejected, never panicked on: an unterminated
    // backtick, a name with no type, an empty element, unbalanced
    // parentheses, an unknown element type, and a trailing lone backslash.
    for bad in [
        "Tuple(`a Int8)",
        "Tuple(`a`)",
        "Tuple(a )",
        "Tuple(Int32,, String)",
        "Tuple(Int32, )",
        "Tuple(Int32))",
        "Tuple(NotAType)",
        "Tuple(`a\\",
    ] {
        assert_eq!(parse_ch_type(bad), None, "should reject {bad:?}");
    }

    // The depth cap applies through Tuple nesting like the other containers.
    let mut deep = String::from("Int8");
    for _ in 0..(MAX_TYPE_DEPTH + 1) {
        deep = format!("Tuple({deep})");
    }
    assert_eq!(parse_ch_type(&deep), None);
}

#[test]
fn test_ch_type_display_round_trips_tuple() {
    // parse(display(t)) == t for every tuple the parser accepts, and
    // display(parse(s)) == s for the canonical server strings.
    for s in [
        "Tuple(Int32, String)",
        "Tuple()",
        "Tuple(a Int32, b Nullable(String))",
        "Tuple(`a b` Int8, `c,d` Int8, `e\\`f` Int8)",
        "Tuple(`select` Int8, selected Int8)",
        "Tuple(`NULL` Int8, nullable Int8)",
        "Tuple(a Tuple(b Int8), c Array(String))",
        "Nullable(Tuple(Int32, String))",
        "Array(Tuple(Int32, Int32))",
        "Tuple(e Enum8('a,b' = 1), d Decimal(9, 4))",
        "Tuple(lc LowCardinality(String), n Nullable(Int32))",
    ] {
        let parsed = parse_ch_type(s).unwrap_or_else(|| panic!("should parse {s:?}"));
        assert_eq!(parsed.to_string(), s, "display must render canonically");
        assert_eq!(
            parse_ch_type(&parsed.to_string()),
            Some(parsed),
            "round-trip failed for {s:?}"
        );
    }
    // The doubled-backtick escape parses but re-renders in the server's
    // backslash form, so it round-trips by value, not by string.
    let parsed = parse_ch_type("Tuple(`g``h` Int8)").unwrap();
    assert_eq!(parsed.to_string(), "Tuple(`g\\`h` Int8)");
    assert_eq!(parse_ch_type(&parsed.to_string()), Some(parsed));
}

#[test]
fn test_parse_ch_type_map() {
    assert_eq!(
        parse_ch_type("Map(String, Int32)"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        ))
    );
    // Either argument can carry top-level-looking commas inside its own
    // parentheses or quotes; the paren/quote-aware splitter must not split
    // there.
    assert_eq!(
        parse_ch_type("Map(String, Decimal(9, 4))"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            }),
        ))
    );
    // Containers compose: LC key, Nullable value, Array value, nested Map,
    // Map inside Array, Map inside Tuple.
    assert_eq!(
        parse_ch_type("Map(LowCardinality(String), UInt8)"),
        Some(ChType::Map(
            Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            Box::new(ChType::UInt8),
        ))
    );
    assert_eq!(
        parse_ch_type("Map(Int32, Nullable(String))"),
        Some(ChType::Map(
            Box::new(ChType::Int32),
            Box::new(ChType::Nullable(Box::new(ChType::String))),
        ))
    );
    assert_eq!(
        parse_ch_type("Map(String, Map(String, Int32))"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            )),
        ))
    );
    assert_eq!(
        parse_ch_type("Array(Map(String, Int32))"),
        Some(ChType::Array(Box::new(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        ))))
    );
    assert_eq!(
        parse_ch_type("Tuple(m Map(String, Int32))"),
        Some(ChType::Tuple(vec![(
            Some("m".to_string()),
            ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        )]))
    );

    // Malformed or illegal-at-parse-time forms. Nullable(Map) is not
    // constructible (DataTypeMap::canBeInsideNullable() is false), so the
    // Nullable arm rejects it outright.
    for bad in [
        "Map(String)",
        "Map(String, Int32, Int8)",
        "Map()",
        "Map(String, NotAType)",
        "Map(String, Int32))",
        "Nullable(Map(String, Int32))",
    ] {
        assert_eq!(parse_ch_type(bad), None, "should reject {bad:?}");
    }

    // The depth cap applies through Map nesting like the other containers.
    let mut deep = String::from("Int8");
    for _ in 0..(MAX_TYPE_DEPTH + 1) {
        deep = format!("Map(String, {deep})");
    }
    assert_eq!(parse_ch_type(&deep), None);
}

#[test]
fn test_ch_type_display_round_trips_map() {
    for s in [
        "Map(String, Int32)",
        "Map(LowCardinality(String), UInt8)",
        "Map(Int32, Nullable(String))",
        "Map(String, Array(Int32))",
        "Map(String, Map(String, Int32))",
        "Array(Map(String, Int32))",
        "Map(String, Tuple(a Int32, b String))",
    ] {
        let parsed = parse_ch_type(s).unwrap_or_else(|| panic!("should parse {s:?}"));
        assert_eq!(parsed.to_string(), s, "display must render canonically");
        assert_eq!(parse_ch_type(&parsed.to_string()), Some(parsed));
    }
}

#[test]
fn test_parse_ch_type_simple_aggregate_function() {
    // Plain scalar inner.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(sum, Float64)"),
        Some(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::Float64),
        })
    );
    // Function name with parenthesized literal params: the split is on the
    // FIRST top-level comma, so the params stay with the function name.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))"),
        Some(ChType::SimpleAggregateFunction {
            func: "groupArrayLastArray(5)".to_string(),
            inner: Box::new(ChType::Array(Box::new(ChType::UInt64))),
        })
    );
    // Inner Tuple whose own commas sit inside parentheses.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(sumMap, Tuple(Array(Int32), Array(Int64)))"),
        Some(ChType::SimpleAggregateFunction {
            func: "sumMap".to_string(),
            inner: Box::new(ChType::Tuple(vec![
                (None, ChType::Array(Box::new(ChType::Int32))),
                (None, ChType::Array(Box::new(ChType::Int64))),
            ])),
        })
    );
    // A whitelisted underscore-bearing function name.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(anyLast_respect_nulls, String)"),
        Some(ChType::SimpleAggregateFunction {
            func: "anyLast_respect_nulls".to_string(),
            inner: Box::new(ChType::String),
        })
    );
}

#[test]
fn parse_aggregate_function_count_signatures_and_display() {
    let bare = ChType::AggregateFunction {
        function: "count".into(),
        arguments: vec![],
    };
    let nullable_arg = ChType::AggregateFunction {
        function: "count".into(),
        arguments: vec![ChType::Nullable(Box::new(ChType::String))],
    };

    assert_eq!(
        parse_ch_type("AggregateFunction(count)"),
        Some(bare.clone())
    );
    assert_eq!(
        parse_ch_type("AggregateFunction(count, Nullable(String))"),
        Some(nullable_arg.clone())
    );
    assert_eq!(bare.to_string(), "AggregateFunction(count)");
    assert_eq!(
        nullable_arg.to_string(),
        "AggregateFunction(count, Nullable(String))"
    );
}

#[test]
fn parse_aggregate_function_nothing_uint64_signature_and_display() {
    // count(Nullable(Nothing)) collapses to this canonical name on the wire, so
    // this is the spelling a Native header actually carries.
    let nothing_uint64 = ChType::AggregateFunction {
        function: "nothingUInt64".into(),
        arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
    };
    assert_eq!(
        parse_ch_type("AggregateFunction(nothingUInt64, Nullable(Nothing))"),
        Some(nothing_uint64.clone())
    );
    assert_eq!(
        nothing_uint64.to_string(),
        "AggregateFunction(nothingUInt64, Nullable(Nothing))"
    );
}

#[test]
fn parse_aggregate_function_nothing_null_signature_and_display() {
    // sum(Nullable(Nothing)) collapses to this canonical name on the wire, so
    // this is the spelling a Native header actually carries.
    let nothing_null = ChType::AggregateFunction {
        function: "nothingNull".into(),
        arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
    };
    assert_eq!(
        parse_ch_type("AggregateFunction(nothingNull, Nullable(Nothing))"),
        Some(nothing_null.clone())
    );
    assert_eq!(
        nothing_null.to_string(),
        "AggregateFunction(nothingNull, Nullable(Nothing))"
    );
}

#[test]
fn parse_aggregate_function_sum_signatures_and_display() {
    // Every exact base sum argument accepted by the server is registered in
    // both its plain and Nullable form. The list covers every accumulator
    // promotion and both Enum widths.
    for argument in [
        "Bool",
        "UInt8",
        "UInt16",
        "UInt32",
        "UInt64",
        "Int8",
        "Int16",
        "Int32",
        "Int64",
        "UInt128",
        "Int128",
        "UInt256",
        "Int256",
        "BFloat16",
        "Float32",
        "Float64",
        "Decimal(9, 4)",
        "Decimal(18, 4)",
        "Decimal(38, 4)",
        "Decimal(76, 4)",
        "Enum8('debit' = -3, 'credit' = 7)",
        "Enum16('debit' = -300, 'credit' = 700)",
    ] {
        for type_name in [
            format!("AggregateFunction(sum, {argument})"),
            format!("AggregateFunction(sum, Nullable({argument}))"),
        ] {
            let parsed =
                parse_ch_type(&type_name).unwrap_or_else(|| panic!("rejected {type_name}"));
            assert_eq!(parsed.to_string(), type_name);
        }
    }
}

#[test]
fn parse_aggregate_function_rejects_unregistered_state_layouts() {
    for type_name in [
        "AggregateFunction()",
        "AggregateFunction(countDistinct, UInt64)",
        "AggregateFunction(count, UInt8, UInt16)",
        "AggregateFunction(1, count)",
        "AggregateFunction(count, LowCardinality(Decimal(9, 4)))",
        "AggregateFunction(count, Map(Nullable(UInt8), UInt8))",
        "AggregateFunction(count, Tuple(a UInt8, a UInt8))",
        // count(Nullable(Nothing)) is renamed to nothingUInt64 by the server, so
        // the count spelling is never on the wire and must not parse.
        "AggregateFunction(count, Nullable(Nothing))",
        // nothingUInt64 is confirmed only with the Nullable(Nothing) argument.
        "AggregateFunction(nothingUInt64)",
        "AggregateFunction(nothingUInt64, UInt64)",
        "AggregateFunction(nothingUInt64, Nothing)",
        // This item registers only the canonical Nullable(Nothing) signature.
        "AggregateFunction(nothingNull)",
        "AggregateFunction(nothingNull, UInt64)",
        "AggregateFunction(nothingNull, Nothing)",
        "AggregateFunction(nothingNull, Nullable(Nothing), UInt64)",
        // Exact base sum is unary and excludes non-numeric Nullable inners and
        // every other sum-family function name. Nullable(Nothing) canonicalizes
        // to nothingNull, not to the nullable sum adapter.
        "AggregateFunction(sum)",
        "AggregateFunction(sum, UInt8, UInt16)",
        "AggregateFunction(sum, Nullable(Nothing))",
        "AggregateFunction(sum, Nullable(String))",
        "AggregateFunction(sum, LowCardinality(UInt64))",
        "AggregateFunction(sum, String)",
        "AggregateFunction(sum, DateTime64(3))",
        "AggregateFunction(sum, IPv4)",
        "AggregateFunction(sum, IntervalDay)",
        "AggregateFunction(sumWithOverflow, UInt64)",
        "Nullable(AggregateFunction(count))",
    ] {
        assert_eq!(parse_ch_type(type_name), None, "accepted {type_name}");
    }
}

#[test]
fn test_simple_aggregate_function_display_round_trips() {
    for spelling in [
        "SimpleAggregateFunction(sum, Float64)",
        "SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String)))",
        "SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))",
        "SimpleAggregateFunction(sumMap, Tuple(Array(Int32), Array(Int64)))",
    ] {
        let parsed = parse_ch_type(spelling).expect("parses");
        assert_eq!(parsed.to_string(), spelling, "round-trip for {spelling}");
    }
}

#[test]
fn test_parse_simple_aggregate_function_rejections() {
    // Multi-type-arg form: only T1 is load-bearing and it is unobserved, so
    // reject rather than decode a guess.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(sum, Int32, Int64)"),
        None
    );
    // Missing the type argument.
    assert_eq!(parse_ch_type("SimpleAggregateFunction(sum)"), None);
    // A non-identifier-shaped function name.
    assert_eq!(parse_ch_type("SimpleAggregateFunction(1sum, Int32)"), None);
}

#[test]
fn test_parse_simple_aggregate_function_inside_wrappers() {
    // SAF parses at any nesting position: the server emits the SAF spelling
    // verbatim inside wrappers and containers (confirmed live at
    // v26.6.1.1193-stable via CREATE + SELECT ... FORMAT Native hexdump for
    // each of these shapes).
    assert_eq!(
        parse_ch_type("Nullable(SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Nullable(Box::new(
            ChType::SimpleAggregateFunction {
                func: "sum".to_string(),
                inner: Box::new(ChType::UInt64),
            }
        )))
    );
    assert_eq!(
        parse_ch_type("Array(SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Array(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::UInt64),
        })))
    );
    assert_eq!(
        parse_ch_type("LowCardinality(SimpleAggregateFunction(anyLast, String))"),
        Some(ChType::LowCardinality(Box::new(
            ChType::SimpleAggregateFunction {
                func: "anyLast".to_string(),
                inner: Box::new(ChType::String),
            }
        )))
    );
    assert_eq!(
        parse_ch_type("Tuple(v SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Tuple(vec![(
            Some("v".to_string()),
            ChType::SimpleAggregateFunction {
                func: "sum".to_string(),
                inner: Box::new(ChType::UInt64),
            }
        )]))
    );
    assert_eq!(
        parse_ch_type("Map(String, SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::SimpleAggregateFunction {
                func: "sum".to_string(),
                inner: Box::new(ChType::UInt64),
            })
        ))
    );
    // Wrapper legality delegates to the inner: Nullable(SAF(Array(...))) is
    // illegal because Nullable(Array(...)) is (the delegate is an Array), and a
    // SAF whose delegate is a Nullable cannot sit inside another Nullable.
    assert_eq!(
        parse_ch_type("Nullable(SimpleAggregateFunction(groupArrayArray, Array(UInt64)))"),
        None
    );
    assert_eq!(
        parse_ch_type("Nullable(SimpleAggregateFunction(anyLast, Nullable(String)))"),
        None
    );
}

#[test]
fn test_parse_ch_type_geo() {
    assert_eq!(parse_ch_type("Point"), Some(ChType::Geo(GeoKind::Point)));
    assert_eq!(parse_ch_type("Ring"), Some(ChType::Geo(GeoKind::Ring)));
    assert_eq!(
        parse_ch_type("LineString"),
        Some(ChType::Geo(GeoKind::LineString))
    );
    assert_eq!(
        parse_ch_type("MultiLineString"),
        Some(ChType::Geo(GeoKind::MultiLineString))
    );
    assert_eq!(
        parse_ch_type("Polygon"),
        Some(ChType::Geo(GeoKind::Polygon))
    );
    assert_eq!(
        parse_ch_type("MultiPolygon"),
        Some(ChType::Geo(GeoKind::MultiPolygon))
    );
    assert_eq!(
        parse_ch_type("MultiPoint"),
        Some(ChType::Geo(GeoKind::MultiPoint))
    );
}

#[test]
fn test_geo_display_round_trips() {
    for spelling in [
        "Point",
        "Ring",
        "LineString",
        "MultiLineString",
        "Polygon",
        "MultiPolygon",
        "MultiPoint",
    ] {
        assert_eq!(parse_ch_type(spelling).unwrap().to_string(), spelling);
    }
    // A geo type composes inside containers and renders the bare alias.
    assert_eq!(
        parse_ch_type("Array(Point)").unwrap().to_string(),
        "Array(Point)"
    );
    assert_eq!(
        parse_ch_type("Map(Point, MultiPolygon)")
            .unwrap()
            .to_string(),
        "Map(Point, MultiPolygon)"
    );
}

#[test]
fn test_parse_geo_rejects_bad_casing() {
    // Registration is case-sensitive with no aliases.
    assert_eq!(parse_ch_type("point"), None);
    assert_eq!(parse_ch_type("ring"), None);
    assert_eq!(parse_ch_type("POLYGON"), None);
    assert_eq!(parse_ch_type("multipolygon"), None);
    assert_eq!(parse_ch_type("multipoint"), None);
}

#[test]
fn test_geo_underlying_type_expansion() {
    // The one-directional structural mapping, confirmed against
    // DataTypeCustomGeo.
    let point = ChType::Tuple(vec![(None, ChType::Float64), (None, ChType::Float64)]);
    assert_eq!(GeoKind::Point.underlying_type(), point);
    assert_eq!(
        GeoKind::Ring.underlying_type(),
        ChType::Array(Box::new(point.clone()))
    );
    assert_eq!(
        GeoKind::LineString.underlying_type(),
        ChType::Array(Box::new(point.clone()))
    );
    assert_eq!(
        GeoKind::MultiPoint.underlying_type(),
        ChType::Array(Box::new(point.clone()))
    );
    assert_eq!(
        GeoKind::Polygon.underlying_type(),
        ChType::Array(Box::new(ChType::Array(Box::new(point.clone()))))
    );
    assert_eq!(
        GeoKind::MultiLineString.underlying_type(),
        ChType::Array(Box::new(ChType::Array(Box::new(point.clone()))))
    );
    assert_eq!(
        GeoKind::MultiPolygon.underlying_type(),
        ChType::Array(Box::new(ChType::Array(Box::new(ChType::Array(Box::new(
            point
        ))))))
    );
}

#[test]
fn test_parse_geometry_canonicalizes_alias_and_delegates() {
    assert_eq!(parse_ch_type("Geometry"), Some(ChType::Geometry));
    assert_eq!(parse_ch_type("GEOMETRY"), Some(ChType::Geometry));
    assert_eq!(parse_ch_type("geometry"), None);
    assert_eq!(ChType::Geometry.to_string(), "Geometry");

    assert_eq!(
        ChType::Geometry.physical_delegate(),
        Some(crate::schema::geometry_underlying_type().clone())
    );
    assert_eq!(
        crate::schema::geometry_underlying_type(),
        &ChType::Variant(vec![
            ChType::Geo(GeoKind::LineString),
            ChType::Geo(GeoKind::MultiLineString),
            ChType::Geo(GeoKind::MultiPolygon),
            ChType::Geo(GeoKind::Point),
            ChType::Geo(GeoKind::Polygon),
            ChType::Geo(GeoKind::Ring),
            ChType::Geo(GeoKind::MultiPoint),
        ])
    );
}

#[test]
fn test_geometry_wrapper_and_container_legality() {
    assert_eq!(parse_ch_type("Nullable(Geometry)"), None);
    let lc = parse_ch_type("LowCardinality(Geometry)").unwrap();
    assert!(unsupported_header_type_name(&lc).is_some());
    assert_eq!(parse_ch_type("Variant(Geometry, String)"), None);

    assert_eq!(
        parse_ch_type("Array(Geometry)"),
        Some(ChType::Array(Box::new(ChType::Geometry)))
    );
    assert_eq!(
        parse_ch_type("Tuple(g Geometry)"),
        Some(ChType::Tuple(vec![(Some("g".into()), ChType::Geometry)]))
    );
    assert_eq!(
        parse_ch_type("Map(String, Geometry)"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Geometry)
        ))
    );
}

#[test]
fn test_parse_ch_type_nested() {
    assert_eq!(
        parse_ch_type("Nested(a UInt32, b String)"),
        Some(ChType::Nested(vec![
            ("a".to_string(), ChType::UInt32),
            ("b".to_string(), ChType::String),
        ]))
    );
    // A backtick-quoted name and a nested type argument.
    assert_eq!(
        parse_ch_type("Nested(`a b` UInt32, c Array(Nullable(String)))"),
        Some(ChType::Nested(vec![
            ("a b".to_string(), ChType::UInt32),
            (
                "c".to_string(),
                ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
            ),
        ]))
    );
}

#[test]
fn test_nested_display_round_trips() {
    for spelling in [
        "Nested(a UInt32, b String)",
        "Nested(`a b` UInt32, c Array(Nullable(String)))",
        "Nested(inner Nested(x Int32, y Int32))",
    ] {
        assert_eq!(parse_ch_type(spelling).unwrap().to_string(), spelling);
    }
}

#[test]
fn test_parse_nested_rejections() {
    // Element names are mandatory.
    assert_eq!(parse_ch_type("Nested(UInt32)"), None);
    assert_eq!(parse_ch_type("Nested(a UInt32, String)"), None);
    // An empty field list is a parse error.
    assert_eq!(parse_ch_type("Nested()"), None);
}

#[test]
fn test_parse_nullable_geo_legality() {
    // Nullable(Point) is legal (Point is a Tuple, canBeInsideNullable true).
    assert_eq!(
        parse_ch_type("Nullable(Point)"),
        Some(ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))))
    );
    // Nullable of the six Array-based geo kinds is illegal (Array is not
    // nullable-able).
    assert_eq!(parse_ch_type("Nullable(Ring)"), None);
    assert_eq!(parse_ch_type("Nullable(LineString)"), None);
    assert_eq!(parse_ch_type("Nullable(Polygon)"), None);
    assert_eq!(parse_ch_type("Nullable(MultiLineString)"), None);
    assert_eq!(parse_ch_type("Nullable(MultiPolygon)"), None);
    assert_eq!(parse_ch_type("Nullable(MultiPoint)"), None);
    let low_cardinality_multi_point = parse_ch_type("LowCardinality(MultiPoint)").unwrap();
    assert_eq!(
        unsupported_header_type_name(&low_cardinality_multi_point).as_deref(),
        Some("LowCardinality(MultiPoint)")
    );
    // Nullable(Nested) is illegal (it is an Array).
    assert_eq!(parse_ch_type("Nullable(Nested(a UInt32))"), None);
}
