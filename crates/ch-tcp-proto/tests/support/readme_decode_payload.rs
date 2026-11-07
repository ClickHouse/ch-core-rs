/// Decodes one `Event::Data` payload, which is exactly one Native block, as
/// soon as it arrives. A zero-row block still carries the result schema.
fn decode_payload(payload: &[u8], connection: &Connection) -> Result<ColBatch, DecodeError> {
    let mut options = DecodeOptions::default();
    // Known once the Server Hello is parsed, which `connect` guarantees.
    options.protocol_revision = connection.effective_revision().unwrap_or(0);
    let mut reader = ByteReader::new(payload);
    match decode_next_block(&mut reader, &options)? {
        // The engine never emits an empty payload, and the block fills it.
        Some(batch) if reader.remaining() == 0 => Ok(batch),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "not one Native block").into()),
    }
}
