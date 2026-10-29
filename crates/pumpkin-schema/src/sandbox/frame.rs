//! Frames, the unit a session carries.
//!
//! A frame is a message's length and its JSON, then a body's length and its bytes. Both
//! lengths are big-endian `u32`s, and both are capped, so that a reader never allocates more
//! than [`MAX_MESSAGE_BYTES`] plus [`MAX_BODY_BYTES`] for a frame.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The largest message in a frame.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// The largest body in a frame. Files longer than this move in several frames.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Writes one frame.
///
/// # Errors
///
/// Fails if the message or the body is too long, or if writing fails.
pub fn write(writer: &mut impl Write, message: &impl Serialize, body: &[u8]) -> io::Result<()> {
    let message = serde_json::to_vec(message).map_err(io::Error::other)?;
    let message_len = encode_len(message.len(), MAX_MESSAGE_BYTES, "message")?;
    let body_len = encode_len(body.len(), MAX_BODY_BYTES, "body")?;
    let mut frame = Vec::with_capacity(8 + message.len() + body.len());
    frame.extend_from_slice(&message_len);
    frame.extend_from_slice(&message);
    frame.extend_from_slice(&body_len);
    frame.extend_from_slice(body);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Reads one frame, or `None` if the connection ended cleanly between frames.
///
/// # Errors
///
/// Fails if reading fails, if the connection ends inside a frame, if a length is over its cap,
/// or if the message is not a `T`.
pub fn read<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<(T, Vec<u8>)>> {
    let Some(message_len) = read_len(reader, MAX_MESSAGE_BYTES, "message", true)? else {
        return Ok(None);
    };
    let message = read_bytes(reader, message_len)?;
    let message = serde_json::from_slice(&message).map_err(invalid_data)?;
    let Some(body_len) = read_len(reader, MAX_BODY_BYTES, "body", false)? else {
        return Err(io::ErrorKind::UnexpectedEof.into());
    };
    Ok(Some((message, read_bytes(reader, body_len)?)))
}

fn encode_len(len: usize, max: usize, what: &str) -> io::Result<[u8; 4]> {
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the {what} is {len} bytes: a frame's {what} holds at most {max}"),
        ));
    }
    let len = u32::try_from(len).map_err(io::Error::other)?;
    Ok(len.to_be_bytes())
}

/// Reads a length, or `None` if the connection ended before its first byte and `may_end`.
fn read_len(
    reader: &mut impl Read,
    max: usize,
    what: &str,
    may_end: bool,
) -> io::Result<Option<usize>> {
    let mut bytes = [0; 4];
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) if filled == 0 && may_end => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let len = usize::try_from(u32::from_be_bytes(bytes)).map_err(io::Error::other)?;
    if len > max {
        return Err(invalid_data(format!(
            "a frame's {what} is {len} bytes: it holds at most {max}"
        )));
    }
    Ok(Some(len))
}

fn read_bytes(reader: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
