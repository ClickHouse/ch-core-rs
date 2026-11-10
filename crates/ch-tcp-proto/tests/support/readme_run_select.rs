/// Why `run_select` failed. After `Server` the connection is reusable if
/// `connection.state()` is `State::Ready`; after `Io` or `Protocol`, discard
/// it with its socket.
#[derive(Debug)]
enum SelectError {
    Io(io::Error),
    Protocol(ProtocolError),
    Server(Box<ServerException>),
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Protocol(error) => write!(f, "protocol error: {error}"),
            Self::Server(error) => write!(f, "server error {}: {}", error.code, error.message),
        }
    }
}

impl std::error::Error for SelectError {}

impl From<io::Error> for SelectError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProtocolError> for SelectError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// Runs one SELECT on a connection from `connect` and returns the result as
/// one Native byte stream: the Data payloads concatenated in order.
fn run_select<S: Read + Write>(
    socket: &mut S,
    connection: &mut Connection,
    query: Query,
) -> Result<Vec<u8>, SelectError> {
    connection.query(query)?;
    // Flush so a buffered stream does not hold the Query while `read` waits.
    socket.write_all(&connection.take_output())?;
    socket.flush()?;

    let mut native = Vec::new();
    let mut read_rows = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        // EOF before EndOfStream is truncation, reported by `finish`.
        let fed = if read == 0 {
            connection.finish()
        } else {
            connection.feed(&buffer[..read])
        };
        // Drain every event before acting on `fed`. Queued events hold
        // memory proportional to the bytes fed since the last drain.
        let mut done = false;
        let mut failure = None;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::Data(block) => native.extend_from_slice(&block),
                // Progress packets carry increments, not running totals.
                Event::Progress(progress) => read_rows += progress.read_rows,
                Event::ProfileInfo(info) => println!("{} result rows", info.rows),
                // The query failed and the exchange is over.
                Event::Exception(exception) => failure = Some(exception),
                Event::EndOfStream => done = true,
                // Totals, Extremes, Log, and ProfileEvents. HTTP FORMAT
                // Native drops Totals and Extremes, so ignoring them matches
                // HTTP results. `Event` is non-exhaustive, so a wildcard arm
                // is required anyway.
                _ => {}
            }
        }
        // A malformed packet after the Exception in the same chunk failed
        // the connection, so it is not reusable: report that first.
        fed?;
        if let Some(exception) = failure {
            return Err(SelectError::Server(exception));
        }
        if done {
            println!("read {read_rows} rows");
            return Ok(native);
        }
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
    }
}
