/// Runs the handshake and one Ping over a blocking byte stream.
fn connect<S: Read + Write>(
    socket: &mut S,
    hello: ClientHello,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let mut connection = Connection::new(hello)?;
    // The Client Hello, which holds the password until taken.
    socket.write_all(&connection.take_output())?;

    let mut buffer = [0_u8; 8192];
    let mut pinged = false;
    loop {
        let read = match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let fed = if read == 0 {
            connection.finish()
        } else {
            connection.feed(&buffer[..read])
        };
        // Drain every event before acting on the Pong or on `fed`.
        let mut got_pong = false;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::ServerHello(hello) => println!("connected to {}", hello.server_name),
                Event::Pong => got_pong = true,
                // Terminal during the handshake: no event follows it.
                Event::Exception(exception) => {
                    let message = format!("server error {}: {}", exception.code, exception.message);
                    return Err(message.into());
                }
                _ => {}
            }
        }
        // A packet after the Pong in the same chunk may have failed the
        // connection, so check `fed` before reporting success.
        fed?;
        if got_pong {
            return Ok(connection);
        }
        // EOF queues no events, so it is never mistaken for a Pong.
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        if connection.state() == State::Ready && !pinged {
            connection.ping()?;
            pinged = true;
        }
        // After the Server Hello: the addendum, then the Ping.
        socket.write_all(&connection.take_output())?;
    }
}
