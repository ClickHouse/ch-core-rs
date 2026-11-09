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
        [Event::ServerHello(Box::new(ServerHello {
            server_name: "ClickHouse".to_owned(),
            version_major: 26,
            version_minor: 8,
            revision: 54_492,
            effective_revision: ENGINE_REVISION,
            timezone: "UTC".to_owned(),
            display_name: "server".to_owned(),
            version_patch: 1,
        }))]
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
            [Event::ServerHello(Box::new(ServerHello {
                server_name: "ClickHouse".to_owned(),
                version_major: 26,
                version_minor: 8,
                revision,
                effective_revision: revision.min(54_460),
                timezone: timezone.to_owned(),
                display_name: display_name.to_owned(),
                version_patch,
            }))],
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
        // EOF after a protocol failure reports the original cause.
        assert_eq!(engine.finish(), Err(ProtocolError::InvalidVarUInt));
        assert_eq!(engine.finish(), Err(ProtocolError::InvalidVarUInt));
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
    assert_eq!(
        engine.ping(),
        Err(ProtocolError::InvalidState {
            operation: "send Ping before draining completed events",
            state: State::Ready,
        })
    );
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::ServerHello(_)]
    ));
    engine.ping().unwrap();
    assert_eq!(engine.state(), State::AwaitingPong);
    assert_eq!(engine.take_output(), [0, CLIENT_PACKET_PING as u8]);
    engine.feed(&[SERVER_PACKET_PONG as u8]).unwrap();
    assert_eq!(engine.state(), State::Ready);
    assert_eq!(events(&mut engine), [Event::Pong]);
}

#[test]
fn effective_revision_is_known_after_the_server_hello() {
    let mut engine = engine();
    assert_eq!(engine.effective_revision(), None);
    engine.feed(&server_hello(54_492)).unwrap();
    assert_eq!(engine.effective_revision(), Some(ENGINE_REVISION));

    let mut old = Connection::new(client_hello()).unwrap();
    old.feed(&server_hello(54_459)).unwrap();
    assert_eq!(old.effective_revision(), Some(54_459));
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
    // Completed events remain observable, but queued writes are unsafe after
    // a fatal protocol error and are discarded.
    assert!(engine.take_output().is_empty());
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
        [Event::Exception(Box::new(ServerException {
            code: 516,
            name: "DB::Exception".to_owned(),
            message: "Authentication failed".to_owned(),
            stack_trace: "stack".to_owned(),
            has_nested: true,
            truncated: false,
        }))]
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
            [Event::Exception(error)] if error.code == 516
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
                Some(Event::Exception(error)) if error.code == 516
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
        [Event::Exception(error)] if error.code == 516
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
        [Event::ServerHello(_), Event::Exception(error)] if error.code == 79 && error.truncated
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

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

fn framed_int64_block(values: &[i64]) -> Vec<u8> {
    let mut bytes = vec![1, 0, 2];
    bytes.extend_from_slice(&(-1_i32).to_le_bytes());
    bytes.push(0);
    put_varuint(&mut bytes, 1);
    put_varuint(&mut bytes, values.len() as u64);
    put_string(&mut bytes, "n");
    put_string(&mut bytes, "Int64");
    bytes.push(0); // default custom serialization
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn framed_empty_block() -> Vec<u8> {
    let mut bytes = vec![1, 0, 2];
    bytes.extend_from_slice(&(-1_i32).to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes
}

fn data_packet(block: &[u8]) -> Vec<u8> {
    let mut bytes = vec![SERVER_PACKET_DATA as u8, 0];
    bytes.extend_from_slice(block);
    bytes
}

fn table_columns_packet(table_name: &str, description: &str) -> Vec<u8> {
    table_columns_bytes_packet(table_name.as_bytes(), description.as_bytes())
}

fn table_columns_bytes_packet(table_name: &[u8], description: &[u8]) -> Vec<u8> {
    let mut bytes = vec![SERVER_PACKET_TABLE_COLUMNS as u8];
    put_varuint(&mut bytes, table_name.len() as u64);
    bytes.extend_from_slice(table_name);
    put_varuint(&mut bytes, description.len() as u64);
    bytes.extend_from_slice(description);
    bytes
}

fn framed_string_block(length: usize) -> Vec<u8> {
    let mut bytes = vec![1, 0, 2];
    bytes.extend_from_slice(&(-1_i32).to_le_bytes());
    bytes.push(0);
    put_varuint(&mut bytes, 1);
    put_varuint(&mut bytes, 1);
    put_string(&mut bytes, "s");
    put_string(&mut bytes, "String");
    bytes.push(0);
    put_varuint(&mut bytes, length as u64);
    bytes.resize(bytes.len() + length, b'x');
    bytes
}

fn framed_array_string_block(elements: usize, string_length: usize) -> Vec<u8> {
    let mut bytes = vec![1, 0, 2];
    bytes.extend_from_slice(&(-1_i32).to_le_bytes());
    bytes.push(0);
    put_varuint(&mut bytes, 1);
    put_varuint(&mut bytes, 1);
    put_string(&mut bytes, "items");
    put_string(&mut bytes, "Array(String)");
    bytes.push(0);
    bytes.extend_from_slice(&(elements as u64).to_le_bytes());
    for _ in 0..elements {
        put_varuint(&mut bytes, string_length as u64);
        bytes.resize(bytes.len() + string_length, b'x');
    }
    bytes
}

#[test]
fn query_parameter_is_one_quoting_layer_over_http_param_text() {
    // The value is HTTP `param_` text. The engine adds exactly one String
    // Field quoting layer, which `readQueryParameters` undoes at
    // v26.8.1.2041-lts. Containers and `\N` must reach the server verbatim.
    let cases: [(&str, &[u8]); 5] = [
        ("a\\tb", b"'a\\\\tb'"),
        ("it's \\\\", b"'it\\'s \\\\\\\\'"),
        ("\\N", b"'\\\\N'"),
        ("['user_1','user_2']", b"'[\\'user_1\\',\\'user_2\\']'"),
        ("{'k1':13}", b"'{\\'k1\\':13}'"),
    ];
    for (value, quoted) in cases {
        let mut engine = ready_engine();
        engine
            .query(Query::new("SELECT {p:String}").with_parameter("p", value))
            .unwrap();
        let output = engine.take_output();
        let mut expected = Vec::new();
        put_string(&mut expected, "p");
        put_varuint(&mut expected, 2);
        put_varuint(&mut expected, quoted.len() as u64);
        expected.extend_from_slice(quoted);
        put_string(&mut expected, "");
        assert!(
            output
                .windows(expected.len())
                .any(|window| window == expected.as_slice()),
            "{value:?}"
        );
    }
}

#[test]
fn query_queues_request_and_filters_tcp_empty_data_sentinel() {
    let mut engine = ready_engine();
    engine
        .query(
            Query::new("SELECT {value:String}")
                .with_query_id("query_13")
                .with_parameter("value", "user_\\'1\\n"),
        )
        .unwrap();
    assert_eq!(engine.state(), State::Querying);
    let output = engine.take_output();
    assert!(output.starts_with(&[CLIENT_PACKET_QUERY as u8]));
    assert!(output
        .windows(b"'user_\\\\\\'1\\\\n'".len())
        .any(|window| window == b"'user_\\\\\\'1\\\\n'"));
    assert!(output.ends_with(&[
        CLIENT_PACKET_DATA as u8,
        0,
        1,
        0,
        2,
        0xff,
        0xff,
        0xff,
        0xff,
        0,
        0,
        0,
    ]));

    let block = framed_int64_block(&[13, 79]);
    let mut response = data_packet(&block);
    response.extend(data_packet(&framed_empty_block()));
    response.push(SERVER_PACKET_END_OF_STREAM as u8);
    for byte in response {
        engine.feed(&[byte]).unwrap();
    }
    assert_eq!(
        events(&mut engine),
        [Event::Data(block), Event::EndOfStream]
    );
    assert_eq!(engine.state(), State::Ready);
}

// ---------------------------------------------------------------------------
// INSERT
// ---------------------------------------------------------------------------

fn inserting_engine() -> Connection {
    let mut engine = ready_engine();
    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&data_packet(&framed_int64_block(&[]))).unwrap();
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::InsertSchema(_)]
    ));
    assert_eq!(engine.state(), State::Inserting);
    engine
}

#[test]
fn insert_queues_query_and_external_table_terminator() {
    let mut engine = ready_engine();
    engine
        .insert(Query::new("INSERT INTO items VALUES").with_query_id("insert_13"))
        .unwrap();
    assert_eq!(engine.state(), State::AwaitingInsertSchema);
    let output = engine.take_output();
    assert_eq!(output.first(), Some(&(CLIENT_PACKET_QUERY as u8)));
    let mut terminator = Vec::new();
    put_empty_data_packet(&mut terminator);
    assert!(output.ends_with(&terminator));
}

#[test]
fn insert_table_columns_and_schema_are_split_independent() {
    let description = "columns format version: 1\n1 columns:\n`n` Int64\n";
    let schema = framed_int64_block(&[]);
    let mut response = table_columns_packet("", description);
    response.extend(data_packet(&schema));

    let run = |chunks: &[&[u8]]| {
        let mut engine = ready_engine();
        engine
            .insert(Query::new("INSERT INTO items VALUES"))
            .unwrap();
        let _ = engine.take_output();
        for chunk in chunks {
            engine.feed(chunk).unwrap();
        }
        (engine.state(), events(&mut engine))
    };
    let expected = run(&[&response]);
    for split in 0..=response.len() {
        assert_eq!(
            run(&[&response[..split], &response[split..]]),
            expected,
            "split {split}"
        );
    }
    assert_eq!(
        expected,
        (
            State::Inserting,
            vec![
                Event::TableColumns {
                    table_name: Vec::new(),
                    description: description.as_bytes().to_vec(),
                    truncated: false,
                },
                Event::InsertSchema(schema),
            ],
        )
    );
}

#[test]
fn insert_table_columns_are_raw_bounded_and_stream_above_input_limit() {
    let large_len = INPUT_LIMIT + 79;
    let mut description = vec![b'x'; large_len];
    description[13] = 0xff;
    let response = table_columns_bytes_packet(b"\xfe", &description);
    assert!(response.len() > INPUT_LIMIT);

    for read_size in [64, response.len()] {
        // The budget covers both fields: one table-name byte plus the
        // description fit exactly.
        let mut engine = ready_engine().with_max_block_bytes(large_len + 1);
        engine
            .insert(Query::new("INSERT INTO items VALUES"))
            .unwrap();
        let _ = engine.take_output();
        for chunk in response.chunks(read_size) {
            engine.feed(chunk).unwrap();
        }
        let [Event::TableColumns {
            table_name,
            description: actual,
            truncated,
        }]: [Event; 1] = events(&mut engine).try_into().unwrap()
        else {
            panic!("expected TableColumns")
        };
        assert_eq!(table_name, b"\xfe");
        assert_eq!(actual, description);
        assert!(!truncated);
        assert!(table_name.capacity() + actual.capacity() <= large_len + 1);
        assert_eq!(engine.state(), State::AwaitingInsertSchema);
        assert!(engine.input.is_empty());
    }

    let limit = MIN_MAX_BLOCK_BYTES;
    let table_name = vec![b'n'; limit + 13];
    let description = vec![b'd'; limit + 79];
    let schema = framed_int64_block(&[]);
    let mut response = table_columns_bytes_packet(&table_name, &description);
    response.extend(data_packet(&schema));
    let mut engine = ready_engine().with_max_block_bytes(limit);
    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    for chunk in response.chunks(79) {
        engine.feed(chunk).unwrap();
    }
    // The retained table name used the whole shared budget.
    assert_eq!(
        events(&mut engine),
        [
            Event::TableColumns {
                table_name: table_name[..limit].to_vec(),
                description: Vec::new(),
                truncated: true,
            },
            Event::InsertSchema(schema),
        ]
    );
    assert_eq!(engine.state(), State::Inserting);
    engine.insert_data(&framed_int64_block(&[13])).unwrap();
    engine.finish_insert().unwrap();
    let _ = engine.take_output();
    engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
    assert_eq!(events(&mut engine), [Event::EndOfStream]);
    assert_eq!(engine.state(), State::Ready);
}

fn awaiting_insert_schema_engine(limit: usize) -> Connection {
    let mut engine = ready_engine().with_max_block_bytes(limit);
    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine
}

#[test]
fn insert_table_columns_fields_share_one_retention_budget() {
    let limit = MIN_MAX_BLOCK_BYTES;
    let table_name = vec![b'n'; 13];
    let description = vec![b'd'; limit + 79];
    let response = table_columns_bytes_packet(&table_name, &description);
    for read_size in [1, 79, response.len()] {
        let mut engine = awaiting_insert_schema_engine(limit);
        for chunk in response.chunks(read_size) {
            engine.feed(chunk).unwrap();
        }
        let [Event::TableColumns {
            table_name: actual_name,
            description: actual_description,
            truncated,
        }]: [Event; 1] = events(&mut engine).try_into().unwrap()
        else {
            panic!("expected TableColumns")
        };
        assert_eq!(actual_name, table_name, "read size {read_size}");
        assert_eq!(actual_description, description[..limit - 13]);
        assert!(truncated);
        assert!(actual_name.capacity() + actual_description.capacity() <= limit);
    }
}

/// A declared field length above 64 x `max_block_bytes` is corruption and
/// fails as soon as its VarUInt completes, in either field and at any split.
#[test]
fn insert_table_columns_declared_length_bound_fails_fast() {
    // Pinned: the docs, README, and CHANGELOG promise "64 times".
    assert_eq!(TABLE_COLUMNS_DECLARED_FACTOR, 64);
    let limit = MIN_MAX_BLOCK_BYTES;
    let bound = limit * 64;
    for length in [bound as u64 + 1, u64::MAX] {
        let mut table_name_prefix = vec![SERVER_PACKET_TABLE_COLUMNS as u8];
        put_varuint(&mut table_name_prefix, length);
        let mut description_prefix = table_columns_bytes_packet(b"t", b"");
        description_prefix.pop(); // the empty description's zero length
        put_varuint(&mut description_prefix, length);
        for (bytes, field) in [
            (table_name_prefix, "table_columns_table_name"),
            (description_prefix, "table_columns_description"),
        ] {
            for read_size in [1, 2, bytes.len()] {
                let mut engine = awaiting_insert_schema_engine(limit);
                let mut result = Ok(());
                let mut fed = 0;
                for chunk in bytes.chunks(read_size) {
                    fed += chunk.len();
                    result = engine.feed(chunk);
                    if result.is_err() {
                        break;
                    }
                }
                assert_eq!(fed, bytes.len(), "{field} {length} read size {read_size}");
                assert_eq!(
                    result,
                    Err(ProtocolError::StringTooLong {
                        field,
                        length: Some(usize::try_from(length).unwrap_or(usize::MAX)),
                        limit: bound,
                    }),
                    "{field} {length} read size {read_size}"
                );
                assert_eq!(engine.state(), PROTOCOL_FAILED);
                assert!(engine.output.is_empty());
                assert!(events(&mut engine).is_empty());
            }
        }
    }

    // Exactly at the bound the field is accepted, retained up to the budget,
    // and skipped to its end, in both fields.
    let schema = framed_int64_block(&[]);
    let at_bound = vec![b'x'; bound];
    for (table_name, description) in [(&at_bound[..], &b"d"[..]), (&b"t"[..], &at_bound[..])] {
        let mut response = table_columns_bytes_packet(table_name, description);
        response.extend(data_packet(&schema));
        for read_size in [997, response.len()] {
            let mut engine = awaiting_insert_schema_engine(limit);
            for chunk in response.chunks(read_size) {
                engine.feed(chunk).unwrap();
            }
            let kept_name = &table_name[..table_name.len().min(limit)];
            let kept_description = &description[..description.len().min(limit - kept_name.len())];
            assert_eq!(
                events(&mut engine),
                [
                    Event::TableColumns {
                        table_name: kept_name.to_vec(),
                        description: kept_description.to_vec(),
                        truncated: true,
                    },
                    Event::InsertSchema(schema.clone()),
                ],
                "read size {read_size}"
            );
            assert_eq!(engine.state(), State::Inserting);
        }
    }
}

/// The declared bound saturates instead of overflowing for a huge cap.
#[test]
fn insert_table_columns_declared_bound_saturates() {
    let progress = TableColumnsProgress::new(usize::MAX);
    assert_eq!(progress.declared_limit, usize::MAX);
    let progress = TableColumnsProgress::new(DEFAULT_MAX_BLOCK_BYTES);
    assert_eq!(
        progress.declared_limit,
        DEFAULT_MAX_BLOCK_BYTES.saturating_mul(TABLE_COLUMNS_DECLARED_FACTOR)
    );
}

#[test]
fn insert_data_validates_one_nonempty_block_atomically() {
    let mut engine = inserting_engine();
    let valid = framed_int64_block(&[13, 79]);
    engine.insert_data(&valid).unwrap();
    let queued = engine.output.clone();
    assert_eq!(queued[..2], [CLIENT_PACKET_DATA as u8, 0]);
    assert_eq!(&queued[2..], valid.as_slice());

    let mut concatenated = valid.clone();
    concatenated.extend(framed_int64_block(&[179]));
    let empty = framed_empty_block();
    let mut zero_columns_with_rows = framed_empty_block();
    *zero_columns_with_rows.last_mut().unwrap() = 1;
    let cases = [
        (
            &valid[..valid.len() - 1],
            ProtocolError::InvalidInsertBlock(InsertBlockError::Truncated),
        ),
        (
            concatenated.as_slice(),
            ProtocolError::InvalidInsertBlock(InsertBlockError::TrailingBytes {
                consumed: valid.len(),
                len: concatenated.len(),
            }),
        ),
        (
            empty.as_slice(),
            ProtocolError::InvalidInsertBlock(InsertBlockError::NoColumns),
        ),
        (
            zero_columns_with_rows.as_slice(),
            ProtocolError::InvalidInsertBlock(InsertBlockError::NoColumns),
        ),
    ];
    for (payload, expected) in cases {
        assert_eq!(engine.insert_data(payload), Err(expected));
        assert_eq!(engine.output, queued);
        assert_eq!(engine.state(), State::Inserting);
    }
}

#[test]
fn outbound_insert_cap_rejection_is_atomic_and_recoverable() {
    let mut engine = inserting_engine().with_max_block_bytes(MIN_MAX_BLOCK_BYTES);
    let before = engine.output.clone();
    let oversized = framed_string_block(MIN_MAX_BLOCK_BYTES + 13);
    assert_eq!(
        engine.insert_data(&oversized),
        Err(ProtocolError::InvalidInsertBlock(
            InsertBlockError::TooLarge {
                len: oversized.len(),
                limit: MIN_MAX_BLOCK_BYTES,
            }
        ))
    );
    assert_eq!(engine.output, before);
    assert_eq!(engine.state(), State::Inserting);

    let valid = framed_int64_block(&[13]);
    engine.insert_data(&valid).unwrap();
    let mut expected = vec![CLIENT_PACKET_DATA as u8, 0];
    expected.extend_from_slice(&valid);
    assert_eq!(engine.take_output(), expected);
}

/// A block exactly at the cap is accepted; one byte over is rejected.
#[test]
fn outbound_insert_cap_accepts_a_block_at_the_limit() {
    let block = framed_string_block(MIN_MAX_BLOCK_BYTES);
    let mut engine = inserting_engine().with_max_block_bytes(block.len());
    let _ = engine.take_output();
    engine.insert_data(&block).unwrap();
    let mut expected = vec![CLIENT_PACKET_DATA as u8, 0];
    expected.extend_from_slice(&block);
    assert_eq!(engine.take_output(), expected);

    let mut engine = inserting_engine().with_max_block_bytes(block.len() - 1);
    assert_eq!(
        engine.insert_data(&block),
        Err(ProtocolError::InvalidInsertBlock(
            InsertBlockError::TooLarge {
                len: block.len(),
                limit: block.len() - 1,
            }
        ))
    );
}

/// An empty output queue is reserved exactly; a non-empty one grows
/// amortized, and repeated untaken writes stay in order.
#[test]
fn insert_data_reserves_exactly_into_an_empty_output() {
    let mut engine = inserting_engine();
    let _ = engine.take_output();
    assert_eq!(engine.output.capacity(), 0);
    let block = framed_int64_block(&[13, 79]);
    engine.insert_data(&block).unwrap();
    assert_eq!(engine.output.len(), block.len() + 2);
    assert_eq!(engine.output.capacity(), engine.output.len());

    // A smaller second write into the non-empty queue grows amortized (at
    // least doubling); an exact reservation would grow by only its length.
    let first = engine.output.len();
    let small = framed_int64_block(&[13]);
    assert!(small.len() + 2 < first);
    engine.insert_data(&small).unwrap();
    assert!(engine.output.capacity() >= 2 * first);
    let packet = |block: &[u8]| [&[CLIENT_PACKET_DATA as u8, 0][..], block].concat();
    assert_eq!(
        engine.take_output(),
        [packet(&block), packet(&small)].concat()
    );
}

#[test]
fn queued_insert_schema_is_a_write_barrier_until_consumed() {
    let mut engine = ready_engine();
    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&data_packet(&framed_int64_block(&[]))).unwrap();
    assert_eq!(engine.state(), State::Inserting);
    assert_eq!(engine.barrier_events, 1);
    assert!(matches!(
        engine.insert_data(&framed_int64_block(&[13])),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(
        engine.finish_insert(),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(engine.poll_event(), Some(Event::InsertSchema(_))));
    assert_eq!(engine.barrier_events, 0);
    engine.insert_data(&framed_int64_block(&[13])).unwrap();
}

#[test]
fn insert_finish_is_single_use_and_waits_for_end_of_stream() {
    let mut engine = inserting_engine();
    engine.insert_data(&framed_int64_block(&[13])).unwrap();
    let _ = engine.take_output();
    engine.finish_insert().unwrap();
    assert_eq!(engine.state(), State::FinishingInsert);
    let mut terminator = Vec::new();
    put_empty_data_packet(&mut terminator);
    assert_eq!(engine.take_output(), terminator);
    assert!(matches!(
        engine.finish_insert(),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(
        engine.insert_data(&framed_int64_block(&[79])),
        Err(ProtocolError::InvalidState { .. })
    ));
    engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
    assert_eq!(events(&mut engine), [Event::EndOfStream]);
    assert_eq!(engine.state(), State::Ready);
}

#[test]
fn insert_rejects_end_of_stream_table_columns_and_data_during_upload() {
    let cases = [
        (
            vec![SERVER_PACKET_END_OF_STREAM as u8],
            SERVER_PACKET_END_OF_STREAM,
        ),
        (
            table_columns_packet("", "n Int64"),
            SERVER_PACKET_TABLE_COLUMNS,
        ),
        (data_packet(&framed_int64_block(&[13])), SERVER_PACKET_DATA),
    ];
    for (packet, code) in cases {
        let mut engine = inserting_engine();
        assert_eq!(
            engine.feed(&packet),
            Err(ProtocolError::UnexpectedPacket {
                packet: code,
                state: State::Inserting,
            })
        );
        assert_eq!(engine.state(), PROTOCOL_FAILED);
    }
}

#[test]
fn insert_exception_in_every_phase_fails_and_discards_output() {
    let failure = exception(241, b"DB::Exception", b"insert failed", b"", 0);

    let mut awaiting = ready_engine();
    awaiting
        .insert(Query::new("INSERT INTO missing VALUES"))
        .unwrap();
    assert!(!awaiting.output.is_empty());
    awaiting.feed(&failure).unwrap();
    assert_eq!(awaiting.state(), EXCEPTION_FAILED);
    assert!(awaiting.take_output().is_empty());

    let mut inserting = inserting_engine();
    inserting.insert_data(&framed_int64_block(&[179])).unwrap();
    assert!(!inserting.output.is_empty());
    inserting.feed(&failure).unwrap();
    assert_eq!(inserting.state(), EXCEPTION_FAILED);
    assert!(inserting.take_output().is_empty());

    let mut finishing = inserting_engine();
    finishing.finish_insert().unwrap();
    assert!(!finishing.output.is_empty());
    finishing.feed(&failure).unwrap();
    assert_eq!(finishing.state(), EXCEPTION_FAILED);
    assert!(finishing.take_output().is_empty());

    for engine in [&mut awaiting, &mut inserting, &mut finishing] {
        assert!(matches!(
            events(engine).as_slice(),
            [Event::Exception(error)] if error.code == 241
        ));
    }
}

#[test]
fn insert_rejects_phase_errors_and_truncated_schema() {
    let mut ready = ready_engine();
    assert!(matches!(
        ready.insert_data(&framed_int64_block(&[13])),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(
        ready.finish_insert(),
        Err(ProtocolError::InvalidState { .. })
    ));

    ready
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = ready.take_output();
    let schema = data_packet(&framed_int64_block(&[]));
    ready.feed(&schema[..schema.len() - 1]).unwrap();
    assert_eq!(ready.finish(), Err(ProtocolError::UnexpectedEof));
    assert_eq!(ready.state(), PROTOCOL_FAILED);
}

#[test]
fn insert_rejects_empty_or_nonzero_row_schema() {
    for schema in [framed_empty_block(), framed_int64_block(&[13])] {
        let mut engine = ready_engine();
        engine
            .insert(Query::new("INSERT INTO items VALUES"))
            .unwrap();
        let _ = engine.take_output();
        assert_eq!(
            engine.feed(&data_packet(&schema)),
            Err(ProtocolError::InvalidInsertSchema)
        );
        assert_eq!(engine.state(), PROTOCOL_FAILED);
        assert!(events(&mut engine).is_empty());
    }
}

#[test]
fn insert_accepts_logs_and_profile_events_in_confirmed_phases() {
    let log = framed_int64_block(&[13]);
    let profile = framed_int64_block(&[79]);
    let schema = framed_int64_block(&[]);
    let mut response = vec![SERVER_PACKET_LOG as u8, 0];
    response.extend_from_slice(&log);
    response.extend(data_packet(&schema));

    let mut engine = ready_engine();
    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&response).unwrap();
    assert_eq!(
        events(&mut engine),
        [Event::Log(log), Event::InsertSchema(schema)]
    );
    engine.finish_insert().unwrap();
    let _ = engine.take_output();

    let mut completion = vec![SERVER_PACKET_PROFILE_EVENTS as u8, 0];
    completion.extend_from_slice(&profile);
    completion.push(SERVER_PACKET_END_OF_STREAM as u8);
    engine.feed(&completion).unwrap();
    assert_eq!(
        events(&mut engine),
        [Event::ProfileEvents(profile), Event::EndOfStream]
    );
    assert_eq!(engine.state(), State::Ready);
}

#[test]
fn inline_insert_completion_without_schema_is_split_independent() {
    let profile = framed_int64_block(&[13]);
    let log = framed_int64_block(&[79]);
    let mut response = progress_packet().to_vec();
    response.extend_from_slice(&[SERVER_PACKET_PROFILE_EVENTS as u8, 0]);
    response.extend_from_slice(&profile);
    response.extend_from_slice(&[SERVER_PACKET_LOG as u8, 0]);
    response.extend_from_slice(&log);
    response.push(SERVER_PACKET_END_OF_STREAM as u8);

    let run = |chunks: &[&[u8]]| {
        let mut engine = ready_engine();
        engine
            .insert(Query::new("INSERT INTO items VALUES (13)"))
            .unwrap();
        let request = engine.take_output();
        for chunk in chunks {
            engine.feed(chunk).unwrap();
        }
        assert!(engine.take_output().is_empty());
        (request, engine.state(), events(&mut engine))
    };
    let expected = run(&[&response]);
    for split in 0..=response.len() {
        assert_eq!(
            run(&[&response[..split], &response[split..]]),
            expected,
            "split {split}"
        );
    }
    assert_eq!(expected.1, State::Ready);
    assert_eq!(
        expected.2,
        [
            Event::Progress(Progress {
                read_rows: 1,
                read_bytes: 2,
                total_rows_to_read: 3,
                written_rows: 4,
                written_bytes: 5,
                elapsed_ns: 6,
            }),
            Event::ProfileEvents(profile),
            Event::Log(log),
            Event::EndOfStream,
        ]
    );
}

#[test]
fn inline_insert_via_query_uses_identical_request_and_returns_ready() {
    let request = Query::new("INSERT INTO items VALUES (13)")
        .with_query_id("inline_13")
        .with_setting("log_queries", "1");
    let mut via_query = ready_engine();
    via_query.query(request.clone()).unwrap();
    let query_bytes = via_query.take_output();

    let mut via_insert = ready_engine();
    via_insert.insert(request).unwrap();
    assert_eq!(via_insert.take_output(), query_bytes);

    via_query
        .feed(&[SERVER_PACKET_END_OF_STREAM as u8])
        .unwrap();
    assert_eq!(events(&mut via_query), [Event::EndOfStream]);
    assert_eq!(via_query.state(), State::Ready);
}

#[test]
fn client_fed_insert_started_with_query_fails_fast_on_default_table_columns() {
    let mut engine = ready_engine();
    engine
        .query(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    assert_eq!(
        engine.feed(&table_columns_packet("", "n Int64")),
        Err(ProtocolError::UnexpectedPacket {
            packet: SERVER_PACKET_TABLE_COLUMNS,
            state: State::Querying,
        })
    );
    assert_eq!(engine.state(), PROTOCOL_FAILED);
}

#[test]
fn query_with_table_columns_disabled_treats_insert_schema_as_result_data() {
    let schema = framed_int64_block(&[]);
    let mut engine = ready_engine();
    engine
        .query(
            Query::new("INSERT INTO items VALUES")
                .with_setting("input_format_defaults_for_omitted_fields", "0"),
        )
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&data_packet(&schema)).unwrap();
    assert_eq!(events(&mut engine), [Event::Data(schema)]);
    assert_eq!(engine.state(), State::Querying);
    assert_eq!(engine.finish(), Err(ProtocolError::UnexpectedEof));
}

#[test]
fn insert_can_be_followed_by_select_and_another_insert() {
    let schema = data_packet(&framed_int64_block(&[]));
    let mut engine = ready_engine();

    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&schema).unwrap();
    let _ = events(&mut engine);
    engine.insert_data(&framed_int64_block(&[13])).unwrap();
    engine.finish_insert().unwrap();
    let _ = engine.take_output();
    engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
    assert_eq!(events(&mut engine), [Event::EndOfStream]);
    assert_eq!(engine.state(), State::Ready);

    engine.query(Query::new("SELECT 79")).unwrap();
    let _ = engine.take_output();
    engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
    assert_eq!(events(&mut engine), [Event::EndOfStream]);

    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&schema).unwrap();
    let _ = events(&mut engine);
    engine.insert_data(&framed_int64_block(&[79])).unwrap();
    engine.finish_insert().unwrap();
    let output = engine.take_output();
    assert!(output.starts_with(&[CLIENT_PACKET_DATA as u8, 0]));
    engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
    assert_eq!(events(&mut engine), [Event::EndOfStream]);
    assert_eq!(engine.state(), State::Ready);
}

#[test]
fn consecutive_client_fed_inserts_reuse_the_connection() {
    let schema = data_packet(&framed_int64_block(&[]));
    let mut engine = ready_engine();
    for value in [13, 79] {
        engine
            .insert(Query::new("INSERT INTO items VALUES"))
            .unwrap();
        let request = engine.take_output();
        assert_eq!(request.first(), Some(&(CLIENT_PACKET_QUERY as u8)));
        engine.feed(&schema).unwrap();
        assert!(matches!(engine.poll_event(), Some(Event::InsertSchema(_))));
        engine.insert_data(&framed_int64_block(&[value])).unwrap();
        engine.finish_insert().unwrap();
        let upload = engine.take_output();
        assert!(upload.starts_with(&[CLIENT_PACKET_DATA as u8, 0]));
        engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
        assert_eq!(events(&mut engine), [Event::EndOfStream]);
        assert_eq!(engine.state(), State::Ready);
    }
}

#[test]
fn insert_writes_may_interleave_with_partial_or_queued_telemetry() {
    let block = framed_int64_block(&[13]);
    for packet_code in [SERVER_PACKET_LOG, SERVER_PACKET_PROFILE_EVENTS] {
        let telemetry_block = framed_int64_block(&[79]);
        let mut telemetry = vec![packet_code as u8, 0];
        telemetry.extend_from_slice(&telemetry_block);
        for split in 0..=telemetry.len() {
            let (head, tail) = telemetry.split_at(split);

            let mut upload = inserting_engine();
            upload.feed(head).unwrap();
            upload.insert_data(&block).unwrap();
            upload.feed(tail).unwrap();
            assert!(!upload.take_output().is_empty(), "upload split {split}");

            let mut finish = inserting_engine();
            finish.feed(head).unwrap();
            finish.finish_insert().unwrap();
            let mut completion = tail.to_vec();
            completion.push(SERVER_PACKET_END_OF_STREAM as u8);
            finish.feed(&completion).unwrap();
            assert_eq!(finish.state(), State::Ready, "finish split {split}");
            assert!(matches!(
                events(&mut finish).last(),
                Some(Event::EndOfStream)
            ));
        }
    }
}

#[test]
fn insert_writes_do_not_overtake_partial_exception_or_packet_code() {
    let failure = exception(241, b"DB::Exception", b"insert failed", b"", 0);
    for split in 1..failure.len() {
        let mut engine = inserting_engine();
        engine.feed(&failure[..split]).unwrap();
        assert!(matches!(
            engine.insert_data(&framed_int64_block(&[13])),
            Err(ProtocolError::InvalidState { .. })
        ));
        assert!(matches!(
            engine.finish_insert(),
            Err(ProtocolError::InvalidState { .. })
        ));
        assert!(engine.take_output().is_empty(), "split {split}");
    }

    // An incomplete multi-byte packet code is not known to be telemetry.
    let mut engine = inserting_engine();
    engine.feed(&[0x80]).unwrap();
    assert!(matches!(
        engine.insert_data(&framed_int64_block(&[13])),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(
        engine.finish_insert(),
        Err(ProtocolError::InvalidState { .. })
    ));
}

#[test]
fn malformed_outbound_block_is_recoverable_and_keeps_cause() {
    let mut engine = inserting_engine();
    let before = engine.output.clone();
    let error = engine.insert_data(&[0xff; 16]).unwrap_err();
    let ProtocolError::InvalidInsertBlock(reason) = error else {
        panic!("expected invalid INSERT block")
    };
    assert!(matches!(reason, InsertBlockError::Decode(_)));
    assert_eq!(engine.output, before);
    assert_eq!(engine.state(), State::Inserting);
    let valid = framed_int64_block(&[13]);
    engine.insert_data(&valid).unwrap();
    let mut expected = vec![CLIENT_PACKET_DATA as u8, 0];
    expected.extend_from_slice(&valid);
    assert_eq!(engine.take_output(), expected);
}

/// Each level of the error chain prints only its own text, so a reporter
/// that walks `source()` shows every cause exactly once.
#[test]
fn invalid_insert_block_error_chain_prints_each_cause_once() {
    let mut engine = inserting_engine();
    let error = engine.insert_data(&[0xff; 16]).unwrap_err();
    let mut levels = Vec::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(level) = current {
        levels.push(level.to_string());
        current = level.source();
    }
    assert_eq!(levels.len(), 3, "{levels:?}");
    assert_eq!(levels[0], "invalid INSERT Native block");
    assert_eq!(levels[1], "Native block scan failed");
    let report = levels.join(": ");
    for level in &levels {
        assert!(!level.is_empty());
        assert_eq!(report.matches(level.as_str()).count(), 1, "{report}");
    }

    let mut engine = inserting_engine().with_max_block_bytes(MIN_MAX_BLOCK_BYTES);
    let oversized = framed_string_block(MIN_MAX_BLOCK_BYTES + 13);
    let error = engine.insert_data(&oversized).unwrap_err();
    let reason = std::error::Error::source(&error).expect("InsertBlockError source");
    assert_eq!(
        reason.to_string(),
        format!(
            "block length {} exceeds the {MIN_MAX_BLOCK_BYTES} byte limit",
            oversized.len()
        )
    );
    assert!(reason.source().is_none());
}

#[test]
fn invalid_insert_block_preserves_the_typed_error_chain() {
    let mut engine = inserting_engine();
    let error = engine.insert_data(&[0xff; 16]).unwrap_err();
    let insert = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<InsertBlockError>())
        .expect("InsertBlockError source");
    assert!(matches!(insert, InsertBlockError::Decode(_)));
    assert!(std::error::Error::source(insert)
        .and_then(|source| source.downcast_ref::<DecodeError>())
        .is_some());
}

#[test]
fn insert_barrier_counter_tracks_mixed_event_queue() {
    let schema = framed_int64_block(&[]);
    let log = framed_int64_block(&[79]);
    let mut response = table_columns_packet("", "n Int64");
    response.extend_from_slice(&[SERVER_PACKET_LOG as u8, 0]);
    response.extend_from_slice(&log);
    response.extend(data_packet(&schema));

    let mut engine = ready_engine();
    engine
        .insert(Query::new("INSERT INTO items VALUES"))
        .unwrap();
    let _ = engine.take_output();
    engine.feed(&response).unwrap();
    assert_eq!(engine.barrier_events, 2);
    assert!(matches!(
        engine.insert_data(&framed_int64_block(&[13])),
        Err(ProtocolError::InvalidState { .. })
    ));
    assert!(matches!(
        engine.poll_event(),
        Some(Event::TableColumns { .. })
    ));
    assert_eq!(engine.barrier_events, 1);
    assert!(matches!(engine.poll_event(), Some(Event::Log(_))));
    assert_eq!(engine.barrier_events, 1);
    assert!(matches!(engine.poll_event(), Some(Event::InsertSchema(_))));
    assert_eq!(engine.barrier_events, 0);
    assert!(engine.poll_event().is_none());
    engine.insert_data(&framed_int64_block(&[13])).unwrap();
}

#[test]
fn protocol_failure_discards_all_queued_output() {
    let mut insert = inserting_engine();
    insert.insert_data(&framed_int64_block(&[13])).unwrap();
    assert!(!insert.output.is_empty());
    assert!(matches!(
        insert.feed(&[SERVER_PACKET_PONG as u8]),
        Err(ProtocolError::UnexpectedPacket { .. })
    ));
    assert!(insert.take_output().is_empty());
    assert_eq!(insert.state(), PROTOCOL_FAILED);

    let mut query = ready_engine();
    query.query(Query::new("SELECT 13")).unwrap();
    assert!(!query.output.is_empty());
    assert!(matches!(
        query.feed(&[SERVER_PACKET_PONG as u8]),
        Err(ProtocolError::UnexpectedPacket { .. })
    ));
    assert!(query.take_output().is_empty());
    assert_eq!(query.state(), PROTOCOL_FAILED);
}

#[test]
fn query_result_packets_are_split_independent_and_typed() {
    let mut block = framed_int64_block(&[13]);
    // Valid non-canonical two-byte encoding of BlockInfo field 1. Splitting
    // between these bytes must remain "partial block", not "clean boundary".
    block.splice(0..1, [0x81, 0x00]);
    let mut response = data_packet(&block);
    response.extend_from_slice(&[SERVER_PACKET_PROGRESS as u8, 1, 2, 3, 4, 5, 6]);
    response.extend_from_slice(&[SERVER_PACKET_PROFILE_INFO as u8, 1, 2, 3, 1, 4, 1]);
    response.push(SERVER_PACKET_END_OF_STREAM as u8);

    let run = |chunks: &[&[u8]]| {
        let mut engine = ready_engine();
        engine.query(Query::new("SELECT 13")).unwrap();
        let _ = engine.take_output();
        for chunk in chunks {
            engine.feed(chunk).unwrap();
        }
        events(&mut engine)
    };
    let expected = run(&[&response]);
    for split in 0..=response.len() {
        assert_eq!(run(&[&response[..split], &response[split..]]), expected);
    }
    assert_eq!(
        expected,
        [
            Event::Data(block),
            Event::Progress(Progress {
                read_rows: 1,
                read_bytes: 2,
                total_rows_to_read: 3,
                written_rows: 4,
                written_bytes: 5,
                elapsed_ns: 6,
            }),
            Event::ProfileInfo(ProfileInfo {
                rows: 1,
                blocks: 2,
                bytes: 3,
                applied_limit: true,
                rows_before_limit: 4,
                unused_obsolete_field: true,
            }),
            Event::EndOfStream,
        ]
    );
}

#[test]
fn query_exception_returns_to_ready_for_reuse() {
    let mut engine = ready_engine();
    engine.query(Query::new("SELECT throwIf(1)")).unwrap();
    let _ = engine.take_output();
    engine
        .feed(&exception(79, b"DB::Exception", b"failed", b"", 0))
        .unwrap();
    assert_eq!(engine.state(), State::Ready);
    assert!(matches!(
        engine.poll_event(),
        Some(Event::Exception(error)) if error.code == 79
    ));
    engine.query(Query::new("SELECT 13")).unwrap();
    assert_eq!(engine.state(), State::Querying);
}

#[test]
fn query_debug_redacts_parameter_values() {
    let query = Query::new("SELECT {value:String}").with_parameter("value", "secret_13");
    let debug = format!("{query:?}");
    assert!(!debug.contains("secret_13"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn terminal_query_exceptions_fail_the_connection() {
    for code in [99, 101, 702, 720, 749] {
        let mut engine = ready_engine();
        engine.query(Query::new("SELECT 13")).unwrap();
        engine
            .feed(&exception(code, b"DB::Exception", b"terminal", b"", 0))
            .unwrap();
        assert_eq!(engine.state(), EXCEPTION_FAILED, "code {code}");
    }
}

/// Invalid known values and unknown names can both fail while Settings are
/// read before the query context exists. The Exception is the first query
/// response, and the server closes the connection after sending it.
#[test]
fn first_exception_after_wire_settings_is_terminal_at_every_split() {
    for (code, message) in [
        (27, b"Cannot parse max_threads".as_slice()),
        (115, b"Unknown setting 'unknown_setting_13'".as_slice()),
        (212, b"Setting value is too large".as_slice()),
        (467, b"Setting value is invalid".as_slice()),
    ] {
        let response = exception(code, b"DB::Exception", message, b"", 0);
        for split in 0..=response.len() {
            let mut engine = ready_engine();
            engine
                .query(Query::new("SELECT 13").with_setting("max_threads", "invalid"))
                .unwrap();
            engine.feed(&[]).unwrap();
            engine.feed(&response[..split]).unwrap();
            engine.feed(&[]).unwrap();
            engine.feed(&response[split..]).unwrap();
            engine.feed(&[]).unwrap();
            assert_eq!(
                engine.state(),
                EXCEPTION_FAILED,
                "code {code} split {split}"
            );
            assert!(matches!(
                events(&mut engine).as_slice(),
                [Event::Exception(error)] if error.code == code
            ));
            assert_eq!(engine.finish(), Ok(()));
        }
    }
}

/// A first-packet SQL syntax error has a query context and is reusable when
/// the query sent no wire settings. Empty feeds and transport splits do not
/// affect that classification.
#[test]
fn first_syntax_exception_without_wire_settings_is_reusable_at_every_split() {
    let response = exception(62, b"DB::Exception", b"Syntax error", b"", 0);
    for split in 0..=response.len() {
        let mut engine = ready_engine();
        engine.query(Query::new("SELEC 13")).unwrap();
        engine.feed(&[]).unwrap();
        engine.feed(&response[..split]).unwrap();
        engine.feed(&[]).unwrap();
        engine.feed(&response[split..]).unwrap();
        engine.feed(&[]).unwrap();
        assert_eq!(engine.state(), State::Ready, "split {split}");
        assert!(matches!(
            events(&mut engine).as_slice(),
            [Event::Exception(error)] if error.code == 62
        ));
    }
}

/// Once any query packet has arrived, an ordinary Exception remains reusable
/// even if the query sent settings. Code 702 remains terminal at any position.
#[test]
fn prior_query_packet_distinguishes_reusable_and_always_terminal_exceptions() {
    for (code, state) in [(62, State::Ready), (702, EXCEPTION_FAILED)] {
        let mut response = progress_packet().to_vec();
        response.extend(exception(code, b"DB::Exception", b"failed", b"", 0));
        for split in 0..=response.len() {
            let mut engine = ready_engine();
            engine
                .query(Query::new("SELECT 13").with_setting("max_threads", "1"))
                .unwrap();
            engine.feed(&response[..split]).unwrap();
            engine.feed(&[]).unwrap();
            engine.feed(&response[split..]).unwrap();
            assert_eq!(engine.state(), state, "code {code} split {split}");
            assert!(matches!(
                events(&mut engine).as_slice(),
                [Event::Progress(_), Event::Exception(error)] if error.code == code
            ));
        }
    }
}

/// Query-local settings and first-packet state are reset between exchanges.
/// The first query has an earlier response packet and is reusable; the second
/// uses a different setting and fails closed on its first response packet.
#[test]
fn consecutive_queries_reset_settings_and_first_packet_state() {
    let mut first_response = progress_packet().to_vec();
    first_response.extend(exception(
        62,
        b"DB::Exception",
        b"first syntax error",
        b"",
        0,
    ));
    let second_response = exception(62, b"DB::Exception", b"second syntax error", b"", 0);

    for first_split in 0..=first_response.len() {
        for second_split in 0..=second_response.len() {
            let mut engine = ready_engine();
            engine
                .query(Query::new("SELEC 13").with_setting("max_threads", "1"))
                .unwrap();
            engine.feed(&first_response[..first_split]).unwrap();
            engine.feed(&first_response[first_split..]).unwrap();
            assert_eq!(engine.state(), State::Ready);
            assert!(matches!(
                events(&mut engine).as_slice(),
                [Event::Progress(_), Event::Exception(error)] if error.code == 62
            ));

            engine
                .query(Query::new("SELEC 79").with_setting("max_block_size", "13"))
                .unwrap();
            engine.feed(&second_response[..second_split]).unwrap();
            engine.feed(&second_response[second_split..]).unwrap();
            assert_eq!(
                engine.state(),
                EXCEPTION_FAILED,
                "first split {first_split}, second split {second_split}"
            );
            assert!(matches!(
                events(&mut engine).as_slice(),
                [Event::Exception(error)] if error.code == 62
            ));
        }
    }
}

/// A completed query that sent settings must not make the next query look as
/// though it also sent settings. A first-packet syntax error without settings
/// remains reusable on the second exchange.
#[test]
fn consecutive_queries_reset_sent_settings_state() {
    let response = exception(62, b"DB::Exception", b"syntax error", b"", 0);
    for split in 0..=response.len() {
        let mut engine = ready_engine();
        engine
            .query(Query::new("SELECT 13").with_setting("max_threads", "1"))
            .unwrap();
        engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
        assert_eq!(engine.state(), State::Ready);
        assert!(matches!(
            events(&mut engine).as_slice(),
            [Event::EndOfStream]
        ));

        engine.query(Query::new("SELEC 79")).unwrap();
        engine.feed(&response[..split]).unwrap();
        engine.feed(&response[split..]).unwrap();
        assert_eq!(engine.state(), State::Ready, "split {split}");
        assert!(matches!(
            events(&mut engine).as_slice(),
            [Event::Exception(error)] if error.code == 62
        ));
    }
}

/// Settings and parameters have no 64 KiB per-field limit: the server reads
/// them with `readStringBinary` at its 1 GiB default. Only the 64 MiB packet
/// bound applies.
#[test]
fn large_setting_and_parameter_values_are_accepted() {
    let mut items = String::from("[");
    let mut number = 0_u64;
    while items.len() < 1024 * 1024 {
        if number > 0 {
            items.push(',');
        }
        items.push_str(&number.to_string());
        number += 1;
    }
    items.push(']');
    let long_setting = "x".repeat(CLIENT_HELLO_STRING_LIMIT + 1);
    let mut engine = ready_engine();
    engine
        .query(
            Query::new("SELECT {items:Array(UInt64)}")
                .with_setting("log_comment", long_setting.as_str())
                .with_parameter("items", items.as_str()),
        )
        .unwrap();
    let output = engine.take_output();
    assert!(output.len() > 1024 * 1024 + CLIENT_HELLO_STRING_LIMIT);
    assert!(output
        .windows(items.len())
        .any(|window| window == items.as_bytes()));

    let mut engine = ready_engine();
    let huge = "1".repeat(QUERY_OUTPUT_LIMIT);
    assert!(matches!(
        engine.query(Query::new("SELECT {v:String}").with_parameter("v", huge)),
        Err(ProtocolError::QueryOutputTooLarge { .. })
    ));
    assert!(engine.take_output().is_empty());
    assert_eq!(engine.state(), State::Ready);
}

/// Setting names stay capped at 64 KiB. Values are bounded only by the whole
/// Query packet limit.
#[test]
fn setting_names_over_the_cap_are_rejected() {
    let at_cap = "n".repeat(SETTING_NAME_LIMIT);
    let mut engine = ready_engine();
    engine
        .query(Query::new("SELECT 13").with_setting(at_cap.as_str(), "1"))
        .unwrap();

    let over = "n".repeat(SETTING_NAME_LIMIT + 1);
    let mut engine = ready_engine();
    assert_eq!(
        engine.query(Query::new("SELECT 13").with_setting(over.as_str(), "1")),
        Err(ProtocolError::StringTooLong {
            field: "setting_name",
            length: Some(SETTING_NAME_LIMIT + 1),
            limit: SETTING_NAME_LIMIT,
        })
    );
    assert!(engine.take_output().is_empty());
    assert_eq!(engine.state(), State::Ready);

    // A first-packet Exception after a setting at the cap is still terminal.
    let mut engine = ready_engine();
    engine
        .query(Query::new("SELECT 13").with_setting(at_cap.as_str(), "1"))
        .unwrap();
    let message = format!("Unknown setting '{at_cap}'");
    engine
        .feed(&exception(
            115,
            b"DB::Exception",
            message.as_bytes(),
            b"",
            0,
        ))
        .unwrap();
    assert_eq!(engine.state(), EXCEPTION_FAILED);
}

/// The packet length is exact: a Query of exactly 64 MiB is accepted and one
/// byte more is rejected with its true length.
#[test]
fn query_output_limit_is_exact() {
    let engine = ready_engine();
    let base = query_packet_len(&Query::new(""), &engine.identity, &engine.quota_key).unwrap();
    let mut sql_len = QUERY_OUTPUT_LIMIT - base;
    let length = |sql_len: usize| {
        let query = Query::new("x".repeat(sql_len));
        query_packet_len(&query, &engine.identity, &engine.quota_key).unwrap()
    };
    // The SQL length prefix grows by a few bytes; step back to fit exactly.
    while length(sql_len) > QUERY_OUTPUT_LIMIT {
        sql_len -= 1;
    }
    assert_eq!(length(sql_len), QUERY_OUTPUT_LIMIT);

    let mut exact = ready_engine();
    exact.query(Query::new("x".repeat(sql_len))).unwrap();
    assert_eq!(exact.take_output().len(), QUERY_OUTPUT_LIMIT);

    let mut over = ready_engine();
    assert_eq!(
        over.query(Query::new("x".repeat(sql_len + 1))),
        Err(ProtocolError::QueryOutputTooLarge {
            length: QUERY_OUTPUT_LIMIT + 1,
            limit: QUERY_OUTPUT_LIMIT,
        })
    );
    assert!(over.take_output().is_empty());
}

/// `query_packet_len` matches the written bytes for every field shape,
/// including multi-byte VarUInts, settings, and escaped parameters.
#[test]
fn query_packet_len_matches_the_written_packet() {
    let long = "q".repeat(200);
    let queries = [
        Query::new("SELECT 1"),
        Query::new(long.as_str())
            .with_query_id(long.as_str())
            .with_os_user("os")
            .with_hostname(long.as_str())
            .with_initial_query_start_time_microseconds(u64::MAX)
            .with_setting("max_threads", "1")
            .with_setting(long.as_str(), long.as_str())
            .with_parameter("p", "it's a\\b")
            .with_parameter("q", long.as_str()),
    ];
    for query in queries {
        let mut engine = ready_engine();
        let expected = query_packet_len(&query, &engine.identity, &engine.quota_key).unwrap();
        engine.query(query).unwrap();
        assert_eq!(engine.take_output().len(), expected);
    }
    let hello = ClientHello::new("c", "u")
        .with_client_version(u64::MAX, 300)
        .with_quota_key("quota_13");
    let mut engine = Connection::new(hello).unwrap();
    engine.feed(&server_hello(ENGINE_REVISION)).unwrap();
    let _ = events(&mut engine);
    let _ = engine.take_output();
    let query = Query::new("SELECT 1").with_setting("a", "b");
    let expected = query_packet_len(&query, &engine.identity, &engine.quota_key).unwrap();
    engine.query(query).unwrap();
    assert_eq!(engine.take_output().len(), expected);
}

/// The Query packet is written straight into the output queue, after any
/// bytes already queued, with no temporary copy.
#[test]
fn query_appends_to_queued_output() {
    let mut engine = ready_engine();
    engine.ping().unwrap();
    engine.feed(&[SERVER_PACKET_PONG as u8]).unwrap();
    let _ = events(&mut engine);
    engine.query(Query::new("SELECT 13")).unwrap();
    let output = engine.take_output();
    assert_eq!(output[0], CLIENT_PACKET_PING as u8);
    assert_eq!(output[1], CLIENT_PACKET_QUERY as u8);
}

#[test]
fn terminal_query_exception_counts_the_bytes_after_it() {
    let mut engine = ready_engine();
    engine.query(Query::new("SELECT 13")).unwrap();
    let mut response = progress_packet().to_vec();
    response.extend(exception(101, b"DB::Exception", b"terminal", b"", 0));
    response.push(SERVER_PACKET_END_OF_STREAM as u8);
    engine.feed(&response).unwrap();
    assert_eq!(engine.state(), EXCEPTION_FAILED);
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::Progress(_), Event::Exception(error)] if error.code == 101
    ));
    assert_eq!(
        engine.finish(),
        Err(ProtocolError::TrailingInput { length: 1 })
    );
}

#[test]
fn query_rejects_repeated_parameter_names_but_not_settings() {
    // TCP keeps the last repeated parameter, HTTP the first, so a repeat is
    // rejected rather than silently diverging. Settings agree on both
    // transports (last wins) and stay accepted.
    let mut ready = ready_engine();
    let error = ready
        .query(
            Query::new("SELECT {p:String}")
                .with_parameter("p", "user_1")
                .with_parameter("q", "user_2")
                .with_parameter("p", "user_2"),
        )
        .unwrap_err();
    assert_eq!(
        error,
        ProtocolError::DuplicateName {
            field: "parameter_name",
            name: "p".to_owned(),
        }
    );
    assert_eq!(
        error.to_string(),
        "parameter_name \"p\" is given more than once"
    );
    assert_eq!(ready.state(), State::Ready);
    assert!(ready.take_output().is_empty());
    ready
        .query(
            Query::new("SELECT {p:String}")
                .with_parameter("p", "user_1")
                .with_setting("max_threads", "1")
                .with_setting("max_threads", "2"),
        )
        .unwrap();
    assert_eq!(ready.state(), State::Querying);
}

#[test]
fn query_rejects_ambiguous_names_and_old_server_revision() {
    let mut ready = ready_engine();
    assert_eq!(
        ready.query(Query::new("SELECT 1").with_setting("", "1")),
        Err(ProtocolError::EmptyName {
            field: "setting_name"
        })
    );
    assert_eq!(
        ready.query(Query::new("SELECT 1").with_parameter("", "1")),
        Err(ProtocolError::EmptyName {
            field: "parameter_name"
        })
    );
    assert_eq!(
        ready.query(
            Query::new("SELECT 1")
                .with_setting("output_format_native_encode_types_in_binary_format", "1")
        ),
        Err(ProtocolError::BinaryNativeTypeEncodingUnsupported)
    );

    let mut old = engine();
    old.feed(&server_hello(54_459)).unwrap();
    let _ = events(&mut old);
    assert_eq!(
        old.query(Query::new("SELECT 1")),
        Err(ProtocolError::UnsupportedQueryRevision { revision: 54_459 })
    );
}

fn progress_packet() -> [u8; 7] {
    [SERVER_PACKET_PROGRESS as u8, 1, 2, 3, 4, 5, 6]
}

/// Round 2 High 1, first repro: the natural loop of feed then drain until
/// `None`, with no extra call, completes a stream of more than 1024 small
/// events followed by EndOfStream, at every read size.
#[test]
fn natural_feed_drain_loop_completes_after_many_small_events() {
    let mut response = progress_packet().repeat(1100);
    response.extend(data_packet(&framed_int64_block(&[13])));
    response.push(SERVER_PACKET_END_OF_STREAM as u8);
    for read_size in [1, 7, 4096, response.len()] {
        let mut engine = ready_engine();
        engine.query(Query::new("SELECT 13")).unwrap();
        let mut seen = Vec::new();
        for chunk in response.chunks(read_size) {
            engine.feed(chunk).unwrap();
            seen.extend(std::iter::from_fn(|| engine.poll_event()));
        }
        assert_eq!(seen.len(), 1102, "read size {read_size}");
        assert_eq!(seen.last(), Some(&Event::EndOfStream));
        assert_eq!(engine.state(), State::Ready, "read size {read_size}");
        assert_eq!(engine.finish(), Ok(()), "read size {read_size}");
    }
}

/// Round 2 High 1, second repro: one feed of 1100 Progress packets, a 5 MiB
/// Data packet, and EndOfStream used to fail with `InputTooLarge`. Any feed
/// size of a well-formed stream must succeed.
#[test]
fn one_feed_of_many_events_and_a_large_block_succeeds() {
    let block = framed_string_block(5 * 1024 * 1024);
    let mut response = progress_packet().repeat(1100);
    response.extend(data_packet(&block));
    response.push(SERVER_PACKET_END_OF_STREAM as u8);
    assert!(response.len() > INPUT_LIMIT);
    let mut engine = ready_engine();
    engine.query(Query::new("SELECT 13")).unwrap();
    assert_eq!(engine.feed(&response), Ok(()));
    assert!(engine.input.is_empty());
    let seen = events(&mut engine);
    assert_eq!(seen.len(), 1102);
    assert_eq!(seen[1100], Event::Data(block));
    assert_eq!(seen[1101], Event::EndOfStream);
    assert_eq!(engine.state(), State::Ready);
}

/// Round 2 High 2: 1023 Progress, EndOfStream, then a pre-query Exception.
/// Once drained the engine is `Failed`, so `query` can never queue a Query
/// that the server will not answer. Before draining, `query` is refused.
#[test]
fn query_is_never_accepted_ahead_of_a_closing_exception() {
    let mut response = progress_packet().repeat(1023);
    response.push(SERVER_PACKET_END_OF_STREAM as u8);
    response.extend(exception(60, b"DB::Exception", b"closing", b"", 0));
    for read_size in [1, 7, 1024, response.len()] {
        let mut engine = ready_engine();
        engine.query(Query::new("SELECT 13")).unwrap();
        let _ = engine.take_output();
        let mut seen = Vec::new();
        for chunk in response.chunks(read_size) {
            engine.feed(chunk).unwrap();
            assert!(
                matches!(
                    engine.query(Query::new("SELECT 79")),
                    Err(ProtocolError::InvalidState { .. })
                ),
                "read size {read_size}"
            );
            seen.extend(std::iter::from_fn(|| engine.poll_event()));
            if engine.state() == State::Ready {
                // Only between EndOfStream and the Exception's packet code.
                assert_eq!(seen.last(), Some(&Event::EndOfStream));
            }
        }
        assert_eq!(engine.state(), EXCEPTION_FAILED, "read size {read_size}");
        assert!(matches!(
            seen.last(),
            Some(Event::Exception(error)) if error.code == 60
        ));
        assert!(matches!(
            engine.query(Query::new("SELECT 79")),
            Err(ProtocolError::InvalidState { .. })
        ));
        assert!(engine.take_output().is_empty(), "read size {read_size}");
    }
}

#[test]
fn query_is_refused_until_events_are_drained() {
    let mut engine = ready_engine();
    engine.query(Query::new("SELECT 13")).unwrap();
    engine.feed(&[SERVER_PACKET_END_OF_STREAM as u8]).unwrap();
    assert_eq!(engine.state(), State::Ready);
    assert_eq!(
        engine.query(Query::new("SELECT 79")),
        Err(ProtocolError::InvalidState {
            operation: "send Query before draining completed events",
            state: State::Ready,
        })
    );
    assert_eq!(events(&mut engine), [Event::EndOfStream]);
    engine.query(Query::new("SELECT 79")).unwrap();
}

#[test]
fn empty_feed_is_a_no_op() {
    let mut engine = ready_engine();
    engine.query(Query::new("SELECT 13")).unwrap();
    let progress = progress_packet();
    engine.feed(&progress[..3]).unwrap();
    engine.feed(&[]).unwrap();
    assert!(events(&mut engine).is_empty());
    engine.feed(&progress[3..]).unwrap();
    assert!(matches!(
        events(&mut engine).as_slice(),
        [Event::Progress(_)]
    ));
}

#[test]
fn queued_payload_limit_is_not_a_query_total_limit() {
    let block = framed_string_block(1024 * 1024);
    let packet = data_packet(&block);
    let mut engine = ready_engine();
    engine
        .query(Query::new("SELECT repeat('x', 1048576)"))
        .unwrap();
    let mut total = 0;
    for _ in 0..65 {
        engine.feed(&packet).unwrap();
        let Some(Event::Data(bytes)) = engine.poll_event() else {
            panic!("expected Data");
        };
        total += bytes.len();
    }
    assert!(total > 64 * 1024 * 1024);
}

#[test]
fn multiple_large_blocks_in_one_feed_fit_the_undrained_queue() {
    let block = framed_string_block(8 * 1024 * 1024);
    let mut response = data_packet(&block);
    response.extend_from_slice(&data_packet(&block));
    let mut engine = ready_engine();
    engine
        .query(Query::new("SELECT repeat('x', 8388608)"))
        .unwrap();
    engine.feed(&response).unwrap();
    assert_eq!(events(&mut engine).len(), 2);
}

#[test]
fn multi_megabyte_nested_block_uses_the_full_second_read() {
    let block = framed_array_string_block(8192, 1024);
    let packet = data_packet(&block);
    assert!(packet.len() > 8 * 1024 * 1024);

    let mut engine = ready_engine();
    engine
        .query(Query::new(
            "SELECT arrayMap(number -> repeat('x', 1024), range(8192))",
        ))
        .unwrap();
    let before = engine.parse_steps;
    engine.feed(&packet[..4096]).unwrap();
    engine.feed(&packet[4096..]).unwrap();
    let steps = engine.parse_steps - before;

    assert_eq!(events(&mut engine), [Event::Data(block)]);
    assert!(
        steps < 10,
        "large second read was artificially resliced into {steps} parse steps"
    );
}

#[test]
fn max_block_bytes_accepts_a_block_at_the_limit_and_rejects_one_byte_more() {
    let block = framed_string_block(2000);
    assert!(block.len() > MIN_MAX_BLOCK_BYTES);
    let packet = data_packet(&block);
    for (limit, accepted) in [(block.len(), true), (block.len() - 1, false)] {
        for read_size in [1, 997, packet.len()] {
            let mut engine = ready_engine().with_max_block_bytes(limit);
            engine.query(Query::new("SELECT 13")).unwrap();
            let _ = engine.take_output();
            let mut result = Ok(());
            for chunk in packet.chunks(read_size) {
                result = engine.feed(chunk);
                if result.is_err() {
                    break;
                }
            }
            if accepted {
                assert_eq!(result, Ok(()), "limit {limit} read size {read_size}");
                assert_eq!(events(&mut engine), [Event::Data(block.clone())]);
            } else {
                assert_eq!(
                    result,
                    Err(ProtocolError::NativeBlockTooLarge { limit }),
                    "limit {limit} read size {read_size}"
                );
                assert_eq!(engine.state(), PROTOCOL_FAILED);
                assert!(engine.input.is_empty());
            }
        }
    }
}

#[test]
fn max_block_bytes_applies_to_every_block_packet_kind() {
    let block = framed_string_block(2000);
    for packet in [
        SERVER_PACKET_DATA,
        SERVER_PACKET_TOTALS,
        SERVER_PACKET_EXTREMES,
        SERVER_PACKET_LOG,
        SERVER_PACKET_PROFILE_EVENTS,
    ] {
        let mut engine = ready_engine().with_max_block_bytes(MIN_MAX_BLOCK_BYTES);
        engine.query(Query::new("SELECT 13")).unwrap();
        let mut bytes = vec![packet as u8, 0];
        bytes.extend_from_slice(&block);
        assert_eq!(
            engine.feed(&bytes),
            Err(ProtocolError::NativeBlockTooLarge {
                limit: MIN_MAX_BLOCK_BYTES
            }),
            "packet {packet}"
        );
    }
}

/// A tiny cap is raised to the documented minimum, so it can never reject
/// the end-of-data marker or a small block.
#[test]
fn max_block_bytes_below_the_minimum_is_raised_to_it() {
    for limit in [0, 1, 8, MIN_MAX_BLOCK_BYTES - 1] {
        let mut engine = ready_engine().with_max_block_bytes(limit);
        assert_eq!(engine.max_block_bytes, MIN_MAX_BLOCK_BYTES);
        engine.query(Query::new("SELECT 13")).unwrap();
        let block = framed_int64_block(&[13]);
        let mut response = data_packet(&block);
        response.extend(data_packet(&framed_empty_block()));
        response.push(SERVER_PACKET_END_OF_STREAM as u8);
        engine.feed(&response).unwrap();
        assert_eq!(
            events(&mut engine),
            [Event::Data(block), Event::EndOfStream]
        );
    }
}

#[test]
fn event_stays_small() {
    // Two words of payload pointer plus tag would be ideal; Progress, six
    // `u64`s, sets the floor. The const assertion in lib.rs enforces it too.
    assert!(std::mem::size_of::<Event>() <= 56);
}

/// The default cap admits a block above the old hard-coded 64 MiB, and block
/// capacity never exceeds the configured limit.
#[test]
fn default_block_cap_admits_blocks_above_64_mib() {
    let block = framed_string_block(65 * 1024 * 1024);
    let packet = data_packet(&block);
    let mut engine = ready_engine();
    engine.query(Query::new("SELECT 13")).unwrap();
    for chunk in packet.chunks(1024 * 1024) {
        engine.feed(chunk).unwrap();
    }
    let [Event::Data(bytes)]: [Event; 1] = events(&mut engine).try_into().unwrap() else {
        panic!("expected Data");
    };
    assert_eq!(bytes.len(), block.len());
    assert!(bytes.capacity() <= DEFAULT_MAX_BLOCK_BYTES);

    let limit = block.len() + 13;
    let mut capped = ready_engine().with_max_block_bytes(limit);
    capped.query(Query::new("SELECT 13")).unwrap();
    for chunk in packet.chunks(1024 * 1024) {
        capped.feed(chunk).unwrap();
    }
    let [Event::Data(bytes)]: [Event; 1] = events(&mut capped).try_into().unwrap() else {
        panic!("expected Data");
    };
    assert!(bytes.capacity() <= limit, "capacity {}", bytes.capacity());
}

#[test]
fn extend_bounded_never_reserves_past_the_limit() {
    let mut buffer = Vec::new();
    for _ in 0..100 {
        extend_bounded(&mut buffer, &[7; 10], 1000);
        assert!(buffer.capacity() <= 1000);
    }
    assert_eq!(buffer.len(), 1000);
}
