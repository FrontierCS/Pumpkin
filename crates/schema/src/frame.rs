//! Frames, the unit that a protocol of Pumpkin's programs carries.
//!
//! A frame is a message's length and its JSON, then a body's length and its bytes. Both
//! lengths are big-endian `u32`s, and both are capped, so that a reader never allocates more
//! than [`MAX_MESSAGE_BYTES`] plus [`MAX_BODY_BYTES`] for a frame. The encoding never changes,
//! as every version of the sandbox protocol keeps it ([`crate::sandbox`]).

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The largest message in a frame.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The largest body in a frame. Files longer than this move in several frames.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Writes one frame, refusing a message or a body over its maximum.
pub fn write(writer: &mut impl Write, message: &impl Serialize, body: &[u8]) -> io::Result<()> {
    let message = serde_json::to_vec(message).map_err(io::Error::other)?;
    let message_len = encode_len(message.len(), MAX_MESSAGE_BYTES, "message")?;
    let body_len = encode_len(body.len(), MAX_BODY_BYTES, "body")?;
    // Keep a small frame a single write. If its data exceeds one maximum message, write the
    // body from the caller's buffer.
    let (inline, separate) = if message.len() + body.len() <= MAX_MESSAGE_BYTES {
        (body, &[][..])
    } else {
        (&[][..], body)
    };
    let mut frame = Vec::with_capacity(8 + message.len() + inline.len());
    frame.extend_from_slice(&message_len);
    frame.extend_from_slice(&message);
    frame.extend_from_slice(&body_len);
    frame.extend_from_slice(inline);
    writer.write_all(&frame)?;
    if !separate.is_empty() {
        writer.write_all(separate)?;
    }
    writer.flush()
}

/// Reads one frame, or `None` if the connection ended cleanly between frames. It refuses a length
/// over its cap, and a message that isn't JSON or is no `T`.
pub fn read<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<(T, Vec<u8>)>> {
    let within = |len: u32, max: usize, what: &str| {
        capped(len, max).ok_or_else(|| {
            invalid_data(format!(
                "a frame's {what} is {len} bytes: it holds at most {max}"
            ))
        })
    };
    let Some(message_len) = read_len(reader)? else {
        return Ok(None);
    };
    let message = read_message(reader, within(message_len, MAX_MESSAGE_BYTES, "message")?)?;
    let Some(body_len) = read_len(reader)? else {
        return Err(io::ErrorKind::UnexpectedEof.into());
    };
    let body = read_bytes(reader, within(body_len, MAX_BODY_BYTES, "body")?)?;
    Ok(Some((message, body)))
}

/// Reads a frame of a message of at most `max` bytes and no body, named as `the` one and as `a`
/// one in what it says of a frame that it refuses.
pub(crate) fn read_bodiless<T: DeserializeOwned>(
    reader: &mut impl Read,
    max: usize,
    [the, a]: [&str; 2],
) -> io::Result<Option<T>> {
    let Some(len) = read_len(reader)? else {
        return Ok(None);
    };
    let over = || invalid_data(format!("{the} is {len} bytes: {a} holds at most {max}"));
    let message = read_message(reader, capped(len, max).ok_or_else(over)?)?;
    match read_len(reader)? {
        Some(0) => Ok(Some(message)),
        Some(len) => Err(invalid_data(format!(
            "{the} has a {len}-byte body: {a} has none"
        ))),
        None => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

fn encode_len(len: usize, max: usize, what: &str) -> io::Result<[u8; 4]> {
    // A length that doesn't fit a u32 is over both caps, of at most 1 MiB.
    match u32::try_from(len) {
        Ok(encoded) if len <= max => Ok(encoded.to_be_bytes()),
        Ok(_) | Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the {what} is {len} bytes: a frame's {what} holds at most {max}"),
        )),
    }
}

/// Reads a length, or `None` if the connection ended before its first byte.
fn read_len(reader: &mut impl Read) -> io::Result<Option<u32>> {
    let mut bytes = [0; 4];
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(u32::from_be_bytes(bytes)))
}

fn capped(len: u32, max: usize) -> Option<usize> {
    // A length that doesn't fit a usize is over the cap, a usize.
    usize::try_from(len).ok().filter(|&len| len <= max)
}

fn read_message<T: DeserializeOwned>(reader: &mut impl Read, len: usize) -> io::Result<T> {
    let message = read_bytes(reader, len)?;
    serde_json::from_slice(&message).map_err(|error| {
        if error.is_syntax() || error.is_eof() {
            invalid_data(format!("a frame's message isn't JSON: {error}"))
        } else {
            invalid_data(error)
        }
    })
}

fn read_bytes(reader: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
