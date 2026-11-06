use super::*;

const EXCEPTION_FAILED: State = State::Failed(FailureKind::ServerException);
const PROTOCOL_FAILED: State = State::Failed(FailureKind::Protocol);

fn client_hello() -> ClientHello {
    ClientHello::new("ch-core-test", "default")
        .with_client_version(26, 8)
        .with_password("secret")
}

/// Convenience builder for tests that are not about the field gates. It
/// shares the effective-revision policy with the code under test, so the
/// gates themselves are pinned by `server_hello_literal_bytes_per_boundary`.
fn server_hello(revision: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    put_varuint(&mut bytes, SERVER_PACKET_HELLO);
    put_string(&mut bytes, "ClickHouse");
    put_varuint(&mut bytes, 26);
    put_varuint(&mut bytes, 8);
    put_varuint(&mut bytes, revision);
    if revision.min(ENGINE_REVISION) >= MIN_REVISION_WITH_SERVER_TIMEZONE {
        put_string(&mut bytes, "UTC");
    }
    if revision.min(ENGINE_REVISION) >= MIN_REVISION_WITH_SERVER_DISPLAY_NAME {
        put_string(&mut bytes, "server");
    }
    if revision.min(ENGINE_REVISION) >= MIN_REVISION_WITH_VERSION_PATCH {
        put_varuint(&mut bytes, 1);
    }
    bytes
}

fn exception(code: i32, name: &[u8], message: &[u8], stack: &[u8], nested: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    put_varuint(&mut bytes, SERVER_PACKET_EXCEPTION);
    bytes.extend_from_slice(&code.to_le_bytes());
    for field in [name, message, stack] {
        put_varuint(&mut bytes, field.len() as u64);
        bytes.extend_from_slice(field);
    }
    bytes.push(nested);
    bytes
}

fn auth_exception() -> Vec<u8> {
    exception(516, b"DB::Exception", b"Authentication failed", b"stack", 0)
}

fn engine() -> Connection {
    let mut engine = Connection::new(client_hello()).unwrap();
    let _ = engine.take_output();
    engine
}

fn ready_engine() -> Connection {
    let mut engine = engine();
    engine.feed(&server_hello(ENGINE_REVISION)).unwrap();
    let _ = engine.take_output();
    let _ = events(&mut engine);
    engine
}

fn events(engine: &mut Connection) -> Vec<Event> {
    std::iter::from_fn(|| engine.poll_event()).collect()
}

/// Everything observable after feeding `chunks` and then calling `finish`.
#[derive(Debug, PartialEq)]
struct Outcome {
    feed: Result<(), ProtocolError>,
    events: Vec<Event>,
    state_before_finish: State,
    finish: Result<(), ProtocolError>,
    state_after_finish: State,
}

fn outcome(mut engine: Connection, chunks: &[&[u8]]) -> Outcome {
    let mut feed = Ok(());
    for chunk in chunks {
        if let Err(error) = engine.feed(chunk) {
            feed = Err(error);
            break;
        }
    }
    let state_before_finish = engine.state();
    let finish = engine.finish();
    Outcome {
        feed,
        events: events(&mut engine),
        state_before_finish,
        finish,
        state_after_finish: engine.state(),
    }
}

// ---------------------------------------------------------------------------
// Client Hello
// ---------------------------------------------------------------------------

#[test]
fn client_hello_debug_redacts_password_and_quota_key() {
    let hello = client_hello().with_quota_key("quota_13");
    let debug = format!("{:?}", hello.clone());
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("quota_13"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn connection_debug_redacts_output_and_quota_key() {
    let engine = Connection::new(client_hello().with_quota_key("quota_13")).unwrap();
    let debug = format!("{engine:?}");
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("quota_13"));
    assert!(debug.contains("bytes redacted"));
    assert!(debug.contains("Handshaking"));
}

#[test]
fn client_hello_output_is_allocated_at_its_exact_size() {
    let long = "p".repeat(300);
    for name in ["c", "ch-core-test"] {
        for password in ["", "secret", long.as_str()] {
            let hello = ClientHello::new(name, "default")
                .with_client_version(u64::MAX, 0)
                .with_password(password);
            let mut engine = Connection::new(hello).unwrap();
            let output = engine.take_output();
            assert_eq!(output.capacity(), output.len(), "{name} {password}");
        }
    }
}

#[test]
fn varuint_len_matches_the_encoding() {
    for value in [0, 1, 0x7f, 0x80, 0x3fff, 0x4000, u64::MAX >> 1, u64::MAX] {
        let mut bytes = Vec::new();
        put_varuint(&mut bytes, value);
        assert_eq!(varuint_len(value), bytes.len(), "value {value}");
    }
}

#[test]
fn client_hello_literal_bytes() {
    let hello = client_hello()
        .with_database("db_1")
        .with_quota_key("quota_13");
    let mut engine = Connection::new(hello).unwrap();
    assert_eq!(
        engine.take_output(),
        b"\x00\x0cch-core-test\x1a\x08\xbc\xa9\x03\x04db_1\x07default\x06secret"
    );
    assert!(engine.take_output().is_empty());
}

#[test]
fn client_side_strings_over_the_server_limit_are_rejected() {
    let long = "x".repeat(CLIENT_HELLO_STRING_LIMIT + 1);
    let shown = Some(CLIENT_HELLO_STRING_LIMIT + 1);
    let cases = [
        (
            "client_name",
            ClientHello::new(long.as_str(), "default"),
            shown,
        ),
        ("user", ClientHello::new("client", long.as_str()), shown),
        (
            "database",
            client_hello().with_database(long.as_str()),
            shown,
        ),
        // Secret lengths are not disclosed.
        (
            "password",
            client_hello().with_password(long.as_str()),
            None,
        ),
        (
            "quota_key",
            client_hello().with_quota_key(long.as_str()),
            None,
        ),
    ];
    for (field, hello, length) in cases {
        let error = Connection::new(hello).err();
        assert_eq!(
            error,
            Some(ProtocolError::StringTooLong {
                field,
                length,
                limit: CLIENT_HELLO_STRING_LIMIT,
            })
        );
        let message = error.unwrap().to_string();
        assert_eq!(
            message.contains(&(CLIENT_HELLO_STRING_LIMIT + 1).to_string()),
            length.is_some(),
            "{message}"
        );
    }
    let exact = "x".repeat(CLIENT_HELLO_STRING_LIMIT);
    assert!(Connection::new(client_hello().with_password(exact)).is_ok());
}

// ---------------------------------------------------------------------------
// Server Hello and addendum
// ---------------------------------------------------------------------------

#[test]
fn server_hello_is_incremental_and_queues_addendum() {
    let bytes = server_hello(54_492);
    let mut engine = engine();
    for byte in bytes {
        engine.feed(&[byte]).unwrap();
    }

    assert_eq!(engine.state(), State::Ready);
    assert_eq!(engine.take_output(), [0]);
    assert_eq!(
        events(&mut engine),
        [Event::ServerHello(ServerHello {
            server_name: "ClickHouse".to_owned(),
            version_major: 26,
            version_minor: 8,
            revision: 54_492,
            effective_revision: ENGINE_REVISION,
            timezone: "UTC".to_owned(),
            display_name: "server".to_owned(),
            version_patch: 1,
        })]
    );
}

/// `(revision, encoded revision, fields after it, timezone, display name,
/// version patch, addendum)`.
type HelloCase = (
    u64,
    &'static [u8],
    &'static [u8],
    &'static str,
    &'static str,
    u64,
    &'static [u8],
);

/// Hand-written bytes for each gate, independent of the parser's policy.
/// `finish` succeeding proves the parser consumed exactly these bytes.
#[test]
fn server_hello_literal_bytes_per_boundary() {
    const PREFIX: &[u8] = b"\x00\x0aClickHouse\x1a\x08";
    const ALL: &[u8] = b"\x03UTC\x06server\x01";
    #[rustfmt::skip]
    let cases: [HelloCase; 11] = [
        (54_057, b"\xa9\xa6\x03", b"", "", "", 54_057, b""),
        (54_058, b"\xaa\xa6\x03", b"\x03UTC", "UTC", "", 54_058, b""),
        (54_371, b"\xe3\xa8\x03", b"\x03UTC", "UTC", "", 54_371, b""),
        (54_372, b"\xe4\xa8\x03", b"\x03UTC\x06server", "UTC", "server", 54_372, b""),
        (54_400, b"\x80\xa9\x03", b"\x03UTC\x06server", "UTC", "server", 54_400, b""),
        (54_401, b"\x81\xa9\x03", ALL, "UTC", "server", 1, b""),
        (54_457, b"\xb9\xa9\x03", ALL, "UTC", "server", 1, b""),
        (54_458, b"\xba\xa9\x03", ALL, "UTC", "server", 1, b"\x00"),
        (54_460, b"\xbc\xa9\x03", ALL, "UTC", "server", 1, b"\x00"),
        (54_461, b"\xbd\xa9\x03", ALL, "UTC", "server", 1, b"\x00"),
        (54_492, b"\xdc\xa9\x03", ALL, "UTC", "server", 1, b"\x00"),
    ];
    for (revision, encoded, tail, timezone, display_name, version_patch, addendum) in cases {
        let mut engine = engine();
        engine.feed(&[PREFIX, encoded, tail].concat()).unwrap();
        assert_eq!(
            events(&mut engine),
            [Event::ServerHello(ServerHello {
                server_name: "ClickHouse".to_owned(),
                version_major: 26,
                version_minor: 8,
                revision,
                effective_revision: revision.min(54_460),
                timezone: timezone.to_owned(),
                display_name: display_name.to_owned(),
                version_patch,
            })],
            "revision {revision}"
        );
        assert_eq!(engine.take_output(), addendum, "revision {revision}");
        assert_eq!(engine.finish(), Ok(()), "revision {revision}");
        assert_eq!(engine.state(), State::Closed, "revision {revision}");
    }
}

#[test]
fn nonempty_quota_key_is_the_complete_addendum() {
    let mut engine = Connection::new(client_hello().with_quota_key("quota_13")).unwrap();
    let _ = engine.take_output();
    engine.feed(&server_hello(ENGINE_REVISION)).unwrap();
    assert_eq!(engine.take_output(), b"\x08quota_13");
}

#[test]
fn max_u64_varuint_is_accepted() {
    let mut bytes = b"\x00\x0aClickHouse".to_vec();
    bytes.extend_from_slice(&[0xff; 9]);
    bytes.push(0x01);
    bytes.extend_from_slice(b"\x08\xbc\xa9\x03\x03UTC\x06server\x01");
    let mut engine = engine();
    engine.feed(&bytes).unwrap();
    let [Event::ServerHello(hello)]: [Event; 1] = events(&mut engine).try_into().unwrap() else {
        panic!("expected ServerHello");
    };
    assert_eq!(hello.version_major, u64::MAX);
}

#[test]
fn rejects_overflowing_varuint() {
    // A 10th byte with the continuation bit, and a 10th byte of 0x02 that
    // would set bit 64.
    for tenth in [0x80, 0x02] {
        let mut bytes = vec![0x80; 9];
        bytes.push(tenth);
        let mut engine = engine();
        assert_eq!(engine.feed(&bytes), Err(ProtocolError::InvalidVarUInt));
        assert_eq!(engine.state(), PROTOCOL_FAILED);
        assert!(matches!(
            engine.finish(),
            Err(ProtocolError::InvalidState { .. })
        ));
    }
}

#[test]
fn rejects_oversized_and_invalid_utf8_server_hello_strings() {
    let mut oversized = vec![SERVER_PACKET_HELLO as u8];
    put_varuint(&mut oversized, (SERVER_HELLO_STRING_LIMIT + 1) as u64);
    let mut oversized_engine = engine();
    assert_eq!(
        oversized_engine.feed(&oversized),
        Err(ProtocolError::StringTooLong {
            field: "server_name",
            length: Some(SERVER_HELLO_STRING_LIMIT + 1),
            limit: SERVER_HELLO_STRING_LIMIT,
        })
    );

    let mut invalid = vec![SERVER_PACKET_HELLO as u8, 1, 0xff];
    put_varuint(&mut invalid, 26);
    put_varuint(&mut invalid, 8);
    put_varuint(&mut invalid, 54_000);
    let mut engine = engine();
    assert_eq!(
        engine.feed(&invalid),
        Err(ProtocolError::InvalidUtf8 {
            field: "server_name"
        })
    );
    assert_eq!(engine.state(), PROTOCOL_FAILED);
}

// ---------------------------------------------------------------------------
// Ping, Pong, and unexpected packets
// ---------------------------------------------------------------------------

#[test]
fn ping_requires_ready_and_pong_returns_to_ready() {
    let mut engine = engine();
    assert!(matches!(
        engine.ping(),
        Err(ProtocolError::InvalidState { .. })
    ));
    engine.feed(&server_hello(ENGINE_REVISION)).unwrap();
    engine.ping().unwrap();
    assert_eq!(engine.state(), State::AwaitingPong);
    assert_eq!(engine.take_output(), [0, CLIENT_PACKET_PING as u8]);
    engine.feed(&[SERVER_PACKET_PONG as u8]).unwrap();
    assert_eq!(engine.state(), State::Ready);
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::ServerHello(_), Event::Pong]
    ));
}

/// W1: a Pong whose packet code straddles a `ping` call must not be taken as
/// the reply. Fed in one chunk, the same bytes are an unexpected packet.
#[test]
fn ping_is_refused_while_a_packet_straddles_and_pong_stays_unexpected() {
    // 0x84 0x00 is packet 4 (Pong) as a two-byte VarUInt.
    let unexpected = Err(ProtocolError::UnexpectedPacket {
        packet: SERVER_PACKET_PONG,
        state: State::Ready,
    });
    let mut whole = ready_engine();
    assert_eq!(whole.feed(&[0x84, 0x00]), unexpected);

    let mut engine = ready_engine();
    engine.feed(&[0x84]).unwrap();
    assert_eq!(
        engine.ping(),
        Err(ProtocolError::InvalidState {
            operation: "send Ping while a server packet is partly received",
            state: State::Ready,
        })
    );
    assert!(engine.take_output().is_empty());
    assert_eq!(engine.feed(&[0x00]), unexpected);
    assert!(events(&mut engine).is_empty());
    assert_eq!(engine.state(), PROTOCOL_FAILED);
}

#[test]
fn packets_outside_their_state_are_unexpected() {
    let pong = [SERVER_PACKET_PONG as u8];
    let hello = server_hello(ENGINE_REVISION);
    let cases: [(Connection, &[u8], u64, State); 4] = [
        (engine(), &pong, SERVER_PACKET_PONG, State::Handshaking),
        (ready_engine(), &pong, SERVER_PACKET_PONG, State::Ready),
        (ready_engine(), &hello, SERVER_PACKET_HELLO, State::Ready),
        (
            {
                let mut engine = ready_engine();
                engine.ping().unwrap();
                engine
            },
            &hello,
            SERVER_PACKET_HELLO,
            State::AwaitingPong,
        ),
    ];
    for (mut engine, bytes, packet, state) in cases {
        assert_eq!(
            engine.feed(bytes),
            Err(ProtocolError::UnexpectedPacket { packet, state })
        );
        assert_eq!(engine.state(), PROTOCOL_FAILED);
        assert!(events(&mut engine).is_empty());
    }
}

#[test]
fn malformed_tail_keeps_completed_event() {
    let mut bytes = server_hello(ENGINE_REVISION);
    bytes.extend_from_slice(&[0x80; 10]);
    let mut engine = engine();
    assert_eq!(engine.feed(&bytes), Err(ProtocolError::InvalidVarUInt));
    assert_eq!(engine.state(), PROTOCOL_FAILED);
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::ServerHello(_)]
    ));
    // The addendum queued before the failure is still in order.
    assert_eq!(engine.take_output(), [0]);
}

// ---------------------------------------------------------------------------
// Exception
// ---------------------------------------------------------------------------

#[test]
fn exception_is_an_event_and_terminal() {
    let mut engine = engine();
    engine
        .feed(&exception(
            516,
            b"DB::Exception",
            b"Authentication failed",
            b"stack",
            1,
        ))
        .unwrap();
    assert_eq!(engine.state(), EXCEPTION_FAILED);
    assert_eq!(
        events(&mut engine),
        [Event::Exception(ServerException {
            code: 516,
            name: "DB::Exception".to_owned(),
            message: "Authentication failed".to_owned(),
            stack_trace: "stack".to_owned(),
            has_nested: true,
            truncated: false,
        })]
    );
    assert!(matches!(
        engine.ping(),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert_eq!(engine.finish(), Ok(()));
    assert_eq!(engine.state(), EXCEPTION_FAILED);
}

#[test]
fn exception_is_accepted_in_ready_and_while_awaiting_pong() {
    for ping in [false, true] {
        let mut engine = ready_engine();
        if ping {
            engine.ping().unwrap();
        }
        engine.feed(&auth_exception()).unwrap();
        assert_eq!(engine.state(), EXCEPTION_FAILED, "ping {ping}");
        assert!(matches!(
            events(&mut engine).as_slice(),
            [Event::Exception(ServerException { code: 516, .. })]
        ));
        assert_eq!(engine.finish(), Ok(()));
    }
}

#[test]
fn non_utf8_exception_text_keeps_the_code() {
    let mut engine = engine();
    engine
        .feed(&exception(
            79,
            b"DB::\xffException",
            b"bad \xfe\xfd text",
            b"\xc3",
            0,
        ))
        .unwrap();
    let [Event::Exception(exception)]: [Event; 1] = events(&mut engine).try_into().unwrap() else {
        panic!("expected Exception");
    };
    assert_eq!(exception.code, 79);
    assert_eq!(exception.name, "DB::\u{fffd}Exception");
    assert_eq!(exception.message, "bad \u{fffd}\u{fffd} text");
    assert_eq!(exception.stack_trace, "\u{fffd}");
    assert!(!exception.truncated);
}

/// C1: an Exception followed by more bytes must give the same feed results,
/// events, state, and finish result for every way of splitting the stream.
#[test]
fn exception_with_trailing_bytes_is_split_independent() {
    let prefixes = [Vec::new(), server_hello(ENGINE_REVISION)];
    let trailers: [&[u8]; 4] = [b"\x00", b"\x04\x04", b"\x80", b"\x02\x01"];
    for prefix in &prefixes {
        for trailer in trailers {
            let stream = [prefix.as_slice(), &auth_exception(), trailer].concat();
            let whole = outcome(engine(), &[&stream]);
            assert_eq!(whole.feed, Ok(()));
            assert_eq!(whole.state_before_finish, EXCEPTION_FAILED);
            assert_eq!(
                whole.finish,
                Err(ProtocolError::TrailingInput {
                    length: trailer.len()
                })
            );
            assert_eq!(whole.state_after_finish, EXCEPTION_FAILED);
            assert!(matches!(
                whole.events.last(),
                Some(Event::Exception(ServerException { code: 516, .. }))
            ));

            for split in 0..=stream.len() {
                let (head, tail) = stream.split_at(split);
                assert_eq!(outcome(engine(), &[head, tail]), whole, "split {split}");
            }
            let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
            assert_eq!(outcome(engine(), &bytewise), whole, "bytewise");
        }
    }
}

#[test]
fn feed_after_finish_is_rejected() {
    let mut engine = engine();
    engine.feed(&auth_exception()).unwrap();
    assert_eq!(engine.finish(), Ok(()));
    assert_eq!(
        engine.feed(b"\x00"),
        Err(ProtocolError::InvalidState {
            operation: "feed input after finish",
            state: EXCEPTION_FAILED,
        })
    );
    assert_eq!(engine.finish(), Ok(()));
}

#[test]
fn oversized_exception_fields_are_truncated_and_keep_the_code() {
    let long = vec![b'm'; EXCEPTION_STRING_LIMIT + 13];
    let stream = [
        exception(79, b"DB::Exception", &long, b"stack", 0),
        b"\x04".to_vec(),
    ]
    .concat();
    let mut expected = None;
    for chunk in [stream.len(), 4096, 1] {
        let chunks: Vec<&[u8]> = stream.chunks(chunk).collect();
        let result = outcome(engine(), &chunks);
        assert_eq!(result.feed, Ok(()), "chunk {chunk}");
        assert_eq!(result.state_before_finish, EXCEPTION_FAILED);
        // The single trailing Pong byte, not the skipped message tail.
        assert_eq!(
            result.finish,
            Err(ProtocolError::TrailingInput { length: 1 })
        );
        let [Event::Exception(exception)] = result.events.as_slice() else {
            panic!("expected Exception at chunk {chunk}");
        };
        assert_eq!(exception.code, 79);
        assert_eq!(exception.name, "DB::Exception");
        assert_eq!(exception.message.len(), EXCEPTION_STRING_LIMIT);
        assert_eq!(exception.stack_trace, "stack");
        assert!(exception.truncated);
        match &expected {
            None => expected = Some(result),
            Some(expected) => assert_eq!(&result, expected, "chunk {chunk}"),
        }
    }
}

#[test]
fn exception_field_at_the_cap_is_not_truncated() {
    for (length, truncated) in [
        (EXCEPTION_STRING_LIMIT, false),
        (EXCEPTION_STRING_LIMIT + 1, true),
    ] {
        let long = vec![b'm'; length];
        let stream = exception(79, b"n", &long, b"stack", 0);
        for chunk in [stream.len(), 65_536, 7] {
            let chunks: Vec<&[u8]> = stream.chunks(chunk).collect();
            let result = outcome(engine(), &chunks);
            assert_eq!(result.feed, Ok(()), "length {length} chunk {chunk}");
            assert_eq!(result.finish, Ok(()), "length {length} chunk {chunk}");
            let [Event::Exception(exception)] = result.events.as_slice() else {
                panic!("expected Exception at length {length} chunk {chunk}");
            };
            assert_eq!(exception.message.len(), EXCEPTION_STRING_LIMIT);
            assert_eq!(exception.stack_trace, "stack");
            assert_eq!(exception.truncated, truncated, "length {length}");
        }
    }
}

/// Fix 2: a declared length that would keep the engine skipping forever is a
/// protocol error as soon as its VarUInt is complete.
#[test]
fn exception_declared_length_over_the_hard_cap_is_rejected() {
    let lengths = [
        (
            EXCEPTION_DECLARED_LIMIT as u64 + 1,
            Some(EXCEPTION_DECLARED_LIMIT + 1),
        ),
        (
            u64::MAX,
            Some(usize::try_from(u64::MAX).unwrap_or(usize::MAX)),
        ),
    ];
    let fields = [
        ("exception_name", 0),
        ("exception_message", 1),
        ("exception_stack_trace", 2),
    ];
    for (declared, reported) in lengths {
        for (field, index) in fields {
            let mut stream = vec![SERVER_PACKET_EXCEPTION as u8];
            stream.extend_from_slice(&79_i32.to_le_bytes());
            for _ in 0..index {
                stream.extend_from_slice(b"\x01x");
            }
            put_varuint(&mut stream, declared);
            // Body bytes that must never be needed.
            stream.extend_from_slice(&[b'x'; 64]);
            let expected = Err(ProtocolError::StringTooLong {
                field,
                length: reported,
                limit: EXCEPTION_DECLARED_LIMIT,
            });
            for split in 0..=stream.len() {
                let (head, tail) = stream.split_at(split);
                let result = outcome(ready_engine(), &[head, tail]);
                assert_eq!(result.feed, expected, "{field} {declared} split {split}");
                assert_eq!(result.state_before_finish, PROTOCOL_FAILED);
                assert!(result.events.is_empty());
            }
            let bytewise: Vec<&[u8]> = stream.chunks(1).collect();
            let result = outcome(ready_engine(), &bytewise);
            assert_eq!(result.feed, expected, "{field} {declared} bytewise");
        }
    }
}

#[test]
fn exception_declared_length_at_the_hard_cap_is_skipped() {
    // Name declared at exactly the hard cap: kept to 1 MiB, the rest skipped.
    let mut stream = vec![SERVER_PACKET_EXCEPTION as u8];
    stream.extend_from_slice(&79_i32.to_le_bytes());
    put_varuint(&mut stream, EXCEPTION_DECLARED_LIMIT as u64);
    stream.resize(stream.len() + EXCEPTION_DECLARED_LIMIT, b'n');
    stream.extend_from_slice(b"\x01m\x01s\x00");
    let mut engine = engine();
    for chunk in stream.chunks(1024 * 1024 + 3) {
        engine.feed(chunk).unwrap();
    }
    let [Event::Exception(exception)]: [Event; 1] = events(&mut engine).try_into().unwrap() else {
        panic!("expected Exception");
    };
    assert_eq!(exception.name.len(), EXCEPTION_STRING_LIMIT);
    assert_eq!(
        (exception.message.as_str(), exception.truncated),
        ("m", true)
    );
    assert_eq!(engine.finish(), Ok(()));
}

#[test]
fn ping_is_rejected_while_an_exception_is_pending() {
    let packet = auth_exception();
    let mut engine = ready_engine();
    // The packet code alone starts the Exception.
    engine.feed(&packet[..1]).unwrap();
    assert_eq!(engine.state(), State::Ready);
    assert_eq!(
        engine.ping(),
        Err(ProtocolError::InvalidState {
            operation: "send Ping while a server packet is partly received",
            state: State::Ready,
        })
    );
    assert!(engine.take_output().is_empty());
    assert_eq!(engine.state(), State::Ready);
    engine.feed(&packet[1..]).unwrap();
    assert_eq!(engine.state(), EXCEPTION_FAILED);
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::Exception(ServerException { code: 516, .. })]
    ));
}

#[test]
fn truncated_exception_prefixes_fail_on_eof() {
    let long = vec![b's'; EXCEPTION_STRING_LIMIT + 1];
    for bytes in [auth_exception(), exception(79, b"n", b"m", &long, 0)] {
        let ends = [
            0,
            1,
            4,
            5,
            9,
            bytes.len() / 2,
            bytes.len() - 2,
            bytes.len() - 1,
        ];
        for end in ends {
            let mut engine = engine();
            engine.feed(&bytes[..end]).unwrap();
            assert_eq!(
                engine.finish(),
                Err(ProtocolError::UnexpectedEof),
                "end {end}"
            );
            assert_eq!(engine.state(), PROTOCOL_FAILED);
        }
    }
}

// ---------------------------------------------------------------------------
// finish and input bounds
// ---------------------------------------------------------------------------

#[test]
fn successful_finish_closes_the_engine() {
    let mut engine = ready_engine();
    engine.finish().unwrap();
    assert_eq!(engine.state(), State::Closed);
    assert_eq!(engine.finish(), Ok(()));
    assert!(matches!(
        engine.feed(&[]),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(
        engine.ping(),
        Err(ProtocolError::InvalidState { .. })
    ));
}

#[test]
fn finish_reports_every_truncated_server_hello_prefix_and_is_stable() {
    let bytes = server_hello(ENGINE_REVISION);
    for length in 0..bytes.len() {
        let mut engine = engine();
        engine.feed(&bytes[..length]).unwrap();
        assert_eq!(engine.finish(), Err(ProtocolError::UnexpectedEof));
        assert_eq!(engine.state(), PROTOCOL_FAILED);
        // N1: a second call reports the same truncation.
        assert_eq!(engine.finish(), Err(ProtocolError::UnexpectedEof));
    }
}

#[test]
fn finish_while_awaiting_pong_is_truncation() {
    let mut engine = ready_engine();
    engine.ping().unwrap();
    assert_eq!(engine.finish(), Err(ProtocolError::UnexpectedEof));
    assert_eq!(engine.finish(), Err(ProtocolError::UnexpectedEof));
}

#[test]
fn large_chunk_with_complete_packets_is_not_rejected_for_size() {
    // More than INPUT_LIMIT in one chunk: a Hello, an Exception whose message
    // exceeds the field cap, and trailing bytes. Nothing is retained.
    let long = vec![b'm'; INPUT_LIMIT];
    let stream = [
        server_hello(ENGINE_REVISION),
        exception(79, b"DB::Exception", &long, b"stack", 0),
        vec![0; 1024 * 1024],
    ]
    .concat();
    assert!(stream.len() > 5 * 1024 * 1024);
    let mut engine = engine();
    assert_eq!(engine.feed(&stream), Ok(()));
    assert!(matches!(
        events(&mut engine).as_slice(),
        [
            Event::ServerHello(_),
            Event::Exception(ServerException {
                code: 79,
                truncated: true,
                ..
            })
        ]
    ));
    assert_eq!(
        engine.finish(),
        Err(ProtocolError::TrailingInput {
            length: 1024 * 1024
        })
    );
}

#[test]
fn input_limit_applies_to_retained_bytes() {
    // A complete Hello larger than the limit is fine: nothing is retained.
    let mut engine = engine();
    engine.input_limit = 16;
    assert!(server_hello(ENGINE_REVISION).len() > 16);
    engine.feed(&server_hello(ENGINE_REVISION)).unwrap();
    assert_eq!(engine.state(), State::Ready);

    // A partial message field longer than the limit is retained and fails.
    let packet = exception(79, b"n", &[b'm'; 64], b"", 0);
    let partial = &packet[..40];
    assert_eq!(
        engine.feed(partial),
        Err(ProtocolError::InputTooLarge {
            // Packet code, Int32 code, and the name were committed.
            length: 40 - 1 - 4 - 2,
            limit: 16,
        })
    );
    assert_eq!(engine.state(), PROTOCOL_FAILED);
    assert!(engine.input.is_empty());
}

#[test]
fn straddling_field_is_not_reparsed_until_complete() {
    let packet = exception(79, b"n", &[b'm'; 1000], b"", 0);
    let mut engine = ready_engine();
    // Packet code, code, name, the message length prefix, and 3 bytes.
    engine.feed(&packet[..12]).unwrap();
    let need = engine.need;
    assert_eq!(need, 2 + 1000);
    let steps = engine.parse_steps;
    for byte in &packet[12..500] {
        engine.feed(std::slice::from_ref(byte)).unwrap();
        assert_eq!(engine.need, need);
    }
    // No parse step ran while the message field was short.
    assert_eq!(engine.parse_steps, steps);
    engine.feed(&packet[500..]).unwrap();
    assert_eq!(engine.state(), EXCEPTION_FAILED);
    // The message, stack trace, and nested flag, plus the attempt that found
    // the stack trace missing when the copied message tail ran out.
    assert_eq!(engine.parse_steps, steps + 4);
}

#[test]
fn drained_large_input_buffer_is_shrunk() {
    let long = vec![b'm'; EXCEPTION_STRING_LIMIT];
    let packet = exception(79, b"n", &long, b"stack", 0);
    let mut engine = ready_engine();
    // Leave most of the message straddling, then complete it together with
    // part of the stack trace, so the engine stays live.
    let middle = 600 * 1024;
    engine.feed(&packet[..middle]).unwrap();
    assert!(engine.input.capacity() > INPUT_SHRINK_THRESHOLD);
    let stack_start = packet.len() - 1 - 5 - 1;
    engine.feed(&packet[middle..stack_start + 3]).unwrap();
    assert_eq!(engine.state(), State::Ready);
    assert_eq!(engine.input.len(), 3);
    assert!(engine.input.capacity() <= INPUT_SHRINK_THRESHOLD);
    engine.feed(&packet[stack_start + 3..]).unwrap();
    assert_eq!(engine.state(), EXCEPTION_FAILED);
}
