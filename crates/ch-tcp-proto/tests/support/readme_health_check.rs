/// Sends one Ping on an idle `Ready` connection and waits for its Pong, for
/// example before reusing a pooled connection.
fn health_check<S: Read + Write>(
    socket: &mut S,
    connection: &mut Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    connection.ping()?;
    socket.write_all(&connection.take_output())?;
    socket.flush()?;
    let mut buffer = [0_u8; 1024];
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
        let mut got_pong = false;
        let mut failure = None;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::Pong => got_pong = true,
                // Terminal outside a query: the server is closing.
                Event::Exception(exception) => failure = Some(exception),
                _ => {}
            }
        }
        fed?;
        if let Some(exception) = failure {
            let message = format!("server error {}: {}", exception.code, exception.message);
            return Err(message.into());
        }
        if got_pong {
            return Ok(());
        }
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
    }
}
