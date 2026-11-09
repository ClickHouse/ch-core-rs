//! Resumable boundary walk for binary runtime type descriptors.
//!
//! This mirrors the grammar of `read_binary_type_inner` and `read_field_literal`.
//! It retains framing progress only. Once complete, the caller runs
//! `read_binary_type` once on the retained descriptor for semantic validation.
//! In particular, enum labels and JSON skip lists can be much larger than the
//! bounded structural type tree and must not be reparsed after every feed.

use super::*;

#[derive(Clone, Copy, Debug)]
enum Element {
    Type(usize),
    NamedType(usize),
    Field(usize),
    Enum(usize),
    String,
}

#[derive(Clone, Copy, Debug)]
enum Task {
    Type(usize),
    Field(usize),
    QBitElement,
    Precision(&'static str),
    Decimal(u8),
    Interval,
    ByteLimit {
        what: &'static str,
        limit: u8,
    },
    VarintLimit {
        what: &'static str,
        limit: usize,
    },
    FixedString,
    QBitDimension,
    Bytes(usize),
    String,
    Varint,
    Count {
        what: &'static str,
        element: Element,
        type_budget: bool,
        limit: usize,
    },
    Repeat {
        count: usize,
        element: Element,
    },
}

/// Boundary state for one descriptor. The caller retains all consumed bytes.
#[derive(Debug)]
pub(crate) struct BinaryTypeScanner {
    tasks: Vec<Task>,
    complexity: usize,
}

impl BinaryTypeScanner {
    pub(crate) fn new() -> Self {
        Self {
            tasks: vec![Task::Type(0)],
            complexity: 0,
        }
    }

    /// Consume newly available descriptor bytes, retaining progress on EOF.
    /// No strings or logical types are allocated by this walk.
    pub(crate) fn scan(&mut self, reader: &mut ByteReader) -> Result<(), BinaryTypeError> {
        while let Some(mut task) = self.tasks.pop() {
            if let Err(error) = self.step(reader, &mut task) {
                if matches!(&error, BinaryTypeError::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof)
                {
                    self.tasks.push(task);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    fn schedule(&mut self, tasks: &[Task]) {
        self.tasks.extend(tasks.iter().rev().copied());
    }

    fn node(&mut self, depth: usize, field: bool) -> Result<(), BinaryTypeError> {
        if depth > MAX_TYPE_DEPTH {
            let prefix = if field {
                "aggregate parameter nesting"
            } else {
                "nesting"
            };
            return Err(BinaryTypeError::Invalid(format!(
                "{prefix} exceeds {MAX_TYPE_DEPTH}"
            )));
        }
        self.complexity = self
            .complexity
            .checked_add(1)
            .ok_or_else(|| BinaryTypeError::Invalid("complexity overflow".into()))?;
        if self.complexity > MAX_BINARY_TYPE_COMPLEXITY {
            return Err(BinaryTypeError::Invalid(format!(
                "complexity exceeds {MAX_BINARY_TYPE_COMPLEXITY}"
            )));
        }
        Ok(())
    }

    fn count(what: &'static str, element: Element, type_budget: bool) -> Task {
        Task::Count {
            what,
            element,
            type_budget,
            limit: MAX_BINARY_TYPE_LIST,
        }
    }

    // VarUInts are at most ten bytes. Probe them atomically so an EOF retains
    // the task without retaining a partially consumed integer or replaying any
    // earlier entries. Length-prefixed payloads use a separate Bytes task.
    fn varint(reader: &mut ByteReader) -> Result<u64, BinaryTypeError> {
        let mut probe = ByteReader::new(reader.remaining_slice());
        let value = probe.read_varint()?;
        reader.skip(probe.position())?;
        Ok(value)
    }

    fn step(&mut self, reader: &mut ByteReader, task: &mut Task) -> Result<(), BinaryTypeError> {
        match *task {
            Task::Bytes(ref mut remaining) => {
                let taken = (*remaining).min(reader.remaining());
                reader.skip(taken)?;
                *remaining -= taken;
                if *remaining != 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
                }
            }
            Task::Precision(name) => {
                read_precision(reader, name)?;
            }
            Task::Decimal(tag) => {
                let mut probe = ByteReader::new(reader.remaining_slice());
                read_decimal(&mut probe, tag)?;
                reader.skip(probe.position())?;
            }
            Task::Interval => {
                read_interval(reader.read_u8()?)?;
            }
            Task::ByteLimit { what, limit } => {
                let value = reader.read_u8()?;
                if value > limit {
                    return Err(BinaryTypeError::Invalid(format!(
                        "{what} {value} exceeds {limit}"
                    )));
                }
            }
            Task::VarintLimit { what, limit } => {
                let value = usize_from_varint(Self::varint(reader)?, what)?;
                if value > limit {
                    return Err(BinaryTypeError::Invalid(format!(
                        "{what} {value} exceeds {limit}"
                    )));
                }
            }
            Task::FixedString => {
                let width = usize_from_varint(Self::varint(reader)?, "FixedString width")?;
                if !(1..=0x00ff_ffff).contains(&width) {
                    return Err(BinaryTypeError::Invalid(format!(
                        "FixedString width {width} is outside 1..=16777215"
                    )));
                }
            }
            Task::QBitDimension => {
                let dimension = usize_from_varint(Self::varint(reader)?, "QBit dimension")?;
                if !(1..=QBIT_MAX_DIMENSION).contains(&dimension) {
                    return Err(BinaryTypeError::Invalid(format!(
                        "QBit dimension {dimension} is outside 1..={QBIT_MAX_DIMENSION}"
                    )));
                }
            }
            Task::Varint => {
                Self::varint(reader)?;
            }
            Task::String => {
                let size = usize_from_varint(Self::varint(reader)?, "string length")?;
                self.tasks.push(Task::Bytes(size));
            }
            Task::Count {
                what,
                element,
                type_budget,
                limit,
            } => {
                let count = usize_from_varint(Self::varint(reader)?, what)?;
                if count > limit {
                    return Err(BinaryTypeError::Invalid(format!(
                        "{what} count {count} exceeds {limit}"
                    )));
                }
                if type_budget {
                    ensure_type_budget(count, self.complexity, what)?;
                }
                self.tasks.push(Task::Repeat { count, element });
            }
            Task::Repeat { count, element } => {
                if count > 0 {
                    self.tasks.push(Task::Repeat {
                        count: count - 1,
                        element,
                    });
                    match element {
                        Element::Type(depth) => self.tasks.push(Task::Type(depth)),
                        Element::NamedType(depth) => {
                            self.schedule(&[Task::String, Task::Type(depth)])
                        }
                        Element::Field(depth) => self.tasks.push(Task::Field(depth)),
                        Element::Enum(width) => self.schedule(&[Task::String, Task::Bytes(width)]),
                        Element::String => self.tasks.push(Task::String),
                    }
                }
            }
            Task::Type(depth) => {
                // Do not charge the node until its tag has actually arrived.
                let tag = reader.read_u8()?;
                self.node(depth, false)?;
                match tag {
                    0x00..=0x11 | 0x15 | 0x1d | 0x28 | 0x29 | 0x2d | 0x31 | 0x32 => {}
                    0x12 | 0x2c => self.tasks.push(Task::String),
                    0x13 => self.tasks.push(Task::Precision("DateTime64")),
                    0x22 => self.tasks.push(Task::Interval),
                    0x2b => self.tasks.push(Task::ByteLimit {
                        what: "Dynamic max_types",
                        limit: 254,
                    }),
                    0x34 => self.tasks.push(Task::Precision("Time64")),
                    0x14 => self.schedule(&[Task::Precision("DateTime64"), Task::String]),
                    0x16 => self.tasks.push(Task::FixedString),
                    0x17 => self
                        .tasks
                        .push(Self::count("Enum8 variants", Element::Enum(1), false)),
                    0x18 => {
                        self.tasks
                            .push(Self::count("Enum16 variants", Element::Enum(2), false))
                    }
                    0x19..=0x1c => self.tasks.push(Task::Decimal(tag)),
                    0x1e | 0x23 | 0x26 => self.tasks.push(Task::Type(depth + 1)),
                    0x1f | 0x20 => self.tasks.push(Self::count(
                        "Tuple elements",
                        if tag == 0x20 {
                            Element::NamedType(depth + 1)
                        } else {
                            Element::Type(depth + 1)
                        },
                        true,
                    )),
                    0x21 => return Err(BinaryTypeError::Unsupported("Set".into())),
                    0x24 => return Err(BinaryTypeError::Unsupported("Function".into())),
                    0x25 | 0x2e => {
                        self.schedule(&[
                            Task::String,
                            Self::count("aggregate parameters", Element::Field(depth + 1), true),
                            Self::count("aggregate arguments", Element::Type(depth + 1), true),
                        ]);
                        if tag == 0x25 {
                            self.tasks.push(Task::Varint);
                        }
                    }
                    0x27 => self.schedule(&[Task::Type(depth + 1), Task::Type(depth + 1)]),
                    0x2a => self.tasks.push(Self::count(
                        "Variant alternatives",
                        Element::Type(depth + 1),
                        true,
                    )),
                    0x2f => self.tasks.push(Self::count(
                        "Nested fields",
                        Element::NamedType(depth + 2),
                        true,
                    )),
                    0x30 => self.schedule(&[
                        Task::ByteLimit {
                            what: "JSON serialization version",
                            limit: 0,
                        },
                        Task::VarintLimit {
                            what: "JSON max_dynamic_paths",
                            limit: JSON_MAX_DYNAMIC_PATHS as usize,
                        },
                        Task::ByteLimit {
                            what: "JSON max_dynamic_types",
                            limit: JSON_MAX_DYNAMIC_TYPES,
                        },
                        Task::Count {
                            what: "JSON typed path",
                            element: Element::NamedType(depth + 1),
                            type_budget: true,
                            limit: JSON_MAX_TYPED_PATHS,
                        },
                        Self::count("JSON skip paths", Element::String, false),
                        Self::count("JSON skip regexps", Element::String, false),
                    ]),
                    0x33 | 0x35 => {
                        return Err(BinaryTypeError::Invalid(format!(
                            "reserved removed type tag 0x{tag:02x}"
                        )))
                    }
                    0x36 => self.schedule(&[Task::QBitElement, Task::QBitDimension]),
                    _ => return Err(BinaryTypeError::Invalid(format!("unknown tag 0x{tag:02x}"))),
                }
            }
            Task::QBitElement => {
                let tag = reader.read_u8()?;
                // QBit's element counts towards complexity but not nesting.
                self.node(0, false)?;
                match tag {
                    0x31 | 0x0d | 0x0e => {}
                    0x2c => self.tasks.push(Task::String),
                    _ => return Err(BinaryTypeError::Invalid(format!("QBit element descriptor tag 0x{tag:02x} is not BFloat16, Float32, or Float64"))),
                }
            }
            Task::Field(depth) => {
                let tag = reader.read_u8()?;
                self.node(depth, true)?;
                match tag {
                    0x00 => {}
                    0x01 | 0x02 => self.tasks.push(Task::Varint),
                    0x07 => self.tasks.push(Task::Bytes(8)),
                    0x0c => self.tasks.push(Task::String),
                    0x0d | 0x0e => self.tasks.push(Self::count(
                        "aggregate field elements",
                        Element::Field(depth + 1),
                        true,
                    )),
                    0x13 => self.tasks.push(Task::Bytes(1)),
                    _ => {
                        return Err(BinaryTypeError::Unsupported(format!(
                            "aggregate Field tag 0x{tag:02x}"
                        )))
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_splits(descriptor: &[u8]) {
        let mut bytes = descriptor.to_vec();
        bytes.extend_from_slice(&[0xff, 0xff]);
        for split in 0..descriptor.len() {
            let mut scanner = BinaryTypeScanner::new();
            let mut first = ByteReader::new(&bytes[..split]);
            assert!(
                matches!(scanner.scan(&mut first), Err(BinaryTypeError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof),
                "split {split}"
            );
            let consumed = first.position();
            let mut rest = ByteReader::new(&bytes[consumed..]);
            scanner.scan(&mut rest).unwrap();
            assert_eq!(
                consumed + rest.position(),
                descriptor.len(),
                "split {split}"
            );
        }
        let mut scanner = BinaryTypeScanner::new();
        let mut consumed = 0;
        for end in 0..descriptor.len() {
            let mut reader = ByteReader::new(&bytes[consumed..end]);
            assert!(
                matches!(scanner.scan(&mut reader), Err(BinaryTypeError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
            );
            consumed += reader.position();
        }
        let mut reader = ByteReader::new(&bytes[consumed..]);
        scanner.scan(&mut reader).unwrap();
        assert_eq!(consumed + reader.position(), descriptor.len());
    }

    #[test]
    fn descriptor_grammar_resumes_at_every_split() {
        for spelling in [
            "String",
            "DateTime('UTC')",
            "DateTime64(3, 'UTC')",
            "Time64(3)",
            "FixedString(130)",
            "Decimal(18, 3)",
            "IntervalDay",
            "Enum8('user_1'=13, 'user_2'=79)",
            "Enum16('user_1'=13, 'user_2'=790)",
            "Array(Nullable(UInt64))",
            "Tuple(String, UInt64)",
            "Tuple(user_1 String, user_2 Array(UInt64))",
            "Map(String, String)",
            "LowCardinality(String)",
            "Variant(String, UInt64)",
            "Dynamic",
            "Nested(user_1 String)",
            "JSON(user_1 String, SKIP user_2, SKIP REGEXP 'user_.*')",
            "AggregateFunction(count)",
            "SimpleAggregateFunction(anyLast, String)",
            "QBit(Float32, 130)",
        ] {
            let ty = parse_ch_type(spelling).unwrap_or_else(|| panic!("{spelling}"));
            let mut bytes = Vec::new();
            write_binary_type(&mut bytes, &ty);
            assert_splits(&bytes);
        }
        // Structural aggregate tags are not emitted by write_binary_type,
        // which uses Custom for aggregate spelling. Cover their raw grammar,
        // including recursive Field literals and the following argument list.
        let signature = [
            1, b'f', // function name
            1,    // one parameter
            0x0d, 3, // Array Field, three elements
            0x0c, 1, b'x', // String Field
            0x0e, 2, 0x00, 0x13, 1, // Tuple Field: NULL, true
            0x07, 0, 0, 0, 0, 0, 0, 0, 0, // Float64 Field
            1, 0x04, // one UInt64 argument
        ];
        let mut aggregate = vec![0x25, 0];
        aggregate.extend_from_slice(&signature);
        assert_splits(&aggregate);
        let mut simple = vec![0x2e];
        simple.extend_from_slice(&signature);
        assert_splits(&simple);
    }

    #[test]
    fn large_enum_label_consumption_is_monotonic() {
        let ty = ChType::Enum16 {
            variants: vec![("x".repeat(16_000), 13), ("user_2".into(), 79)],
        };
        let mut bytes = Vec::new();
        write_binary_type(&mut bytes, &ty);
        let mut scanner = BinaryTypeScanner::new();
        let mut consumed = 0;
        for end in 1..bytes.len() {
            let mut reader = ByteReader::new(&bytes[consumed..end]);
            assert!(
                matches!(scanner.scan(&mut reader), Err(BinaryTypeError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
            );
            consumed += reader.position();
            // Only an incomplete VarUInt can be left unread, at most 9 bytes.
            assert!(end - consumed < 10);
            assert!(scanner.tasks.len() < 5);
        }
        scanner
            .scan(&mut ByteReader::new(&bytes[consumed..]))
            .unwrap();
    }
}

#[cfg(test)]
mod invalid_tests {
    use super::*;

    #[test]
    fn malformed_scalar_metadata_keeps_parser_errors() {
        for bytes in [
            vec![0x13, 10],
            vec![0x14, 10],
            vec![0x34, 10],
            vec![0x19, 0, 0],
            vec![0x19, 9, 10],
            vec![0x22, 11],
            vec![0x2b, 255],
            vec![0x30, 1],
            vec![0x30, 0, 0x91, 0x4e],
            vec![0x30, 0, 0, 255],
            vec![0x16, 0],
            vec![0x36, 0x0d, 0],
            vec![0x36, 0x01],
            vec![0x33],
            vec![0xff],
        ] {
            let expected = read_binary_type(&mut ByteReader::new(&bytes))
                .unwrap_err()
                .to_string();
            let actual = BinaryTypeScanner::new()
                .scan(&mut ByteReader::new(&bytes))
                .unwrap_err()
                .to_string();
            assert_eq!(actual, expected, "{bytes:?}");
        }
    }
}
