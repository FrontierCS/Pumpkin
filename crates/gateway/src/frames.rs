//! Frames as a session reads them from the bytes received so far, and writes them as bytes to
//! send.

use std::io;

use schema::frame::{self, MAX_BODY_BYTES, MAX_MESSAGE_BYTES};
use schema::sandbox::{FromSandbox, HelloReply, ToSandbox};

/// The bytes received that no frame has taken yet.
#[derive(Default)]
pub(crate) struct Inbound {
    bytes: Vec<u8>,
    // How many of `bytes` frames have taken: they go as the next bytes come, so that taking a
    // frame moves no bytes, and a receive moves those left at most once.
    taken: usize,
}

impl Inbound {
    pub(crate) fn extend(&mut self, received: &[u8]) {
        self.bytes.drain(..self.taken);
        self.taken = 0;
        self.bytes.extend_from_slice(received);
    }

    /// The bytes that no frame has taken.
    pub(crate) fn rest(&self) -> &[u8] {
        &self.bytes[self.taken..]
    }

    /// Takes the first frame that it holds whole, as `read` reads it. None while the frame is
    /// incomplete, with lengths within `caps`, the message's and the body's. A length past its
    /// cap goes to `read` at once, which refuses it.
    pub(crate) fn take<T>(
        &mut self,
        caps: (usize, usize),
        read: impl FnOnce(&mut &[u8]) -> io::Result<Option<T>>,
    ) -> io::Result<Option<T>> {
        let inbound = self.rest();
        if !readable(inbound, caps) {
            return Ok(None);
        }
        let mut rest = inbound;
        let taken = read(&mut rest)?;
        self.taken += inbound.len() - rest.len();
        Ok(taken)
    }

    /// Takes the first whole frame of the sandbox's protocol that it holds.
    pub(crate) fn take_frame(&mut self) -> io::Result<Option<(FromSandbox, Vec<u8>)>> {
        self.take((MAX_MESSAGE_BYTES, MAX_BODY_BYTES), |rest| {
            frame::read(rest)
        })
    }
}

/// The bytes of one frame, or why it can't be made: `InvalidInput` for a message or body over
/// its cap, as [`frame::write`] refuses.
pub(crate) fn bytes(message: &ToSandbox, body: &[u8]) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(8 + body.len());
    frame::write(&mut bytes, message, body)?;
    Ok(bytes)
}

/// The bytes of the answer to a hello.
pub(crate) fn hello_reply(reply: &HelloReply) -> Vec<u8> {
    let mut bytes = Vec::new();
    match frame::write(&mut bytes, reply, &[]) {
        Ok(()) => bytes,
        Err(error) => unreachable!(
            "the answer to a hello can't be framed: {error}, though it holds a grant of a fixed \
             size or a refusal's message, which `frame::MAX_MESSAGE_BYTES` far exceeds"
        ),
    }
}

// Whether `inbound` holds a whole frame, or one whose lengths pass `caps`.
fn readable(inbound: &[u8], (max_message, max_body): (usize, usize)) -> bool {
    let len = |at: usize| {
        let bytes = inbound.get(at..at + 4)?;
        let bytes: [u8; 4] = bytes.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(bytes)).ok()
    };
    let Some(message) = len(0) else {
        return false;
    };
    if message > max_message {
        return true;
    }
    let Some(body) = len(4 + message) else {
        return false;
    };
    body > max_body || inbound.len() >= 8 + message + body
}

/// Whether what `inbound` holds, which the stream ended after, is a frame cut short; then `read`
/// says how.
pub(crate) fn cut_short<T>(
    inbound: &[u8],
    read: impl FnOnce(&mut &[u8]) -> io::Result<Option<T>>,
) -> Option<io::Error> {
    if inbound.is_empty() {
        return None;
    }
    let mut rest = inbound;
    match read(&mut rest) {
        Err(error) => Some(error),
        Ok(_) => Some(io::ErrorKind::UnexpectedEof.into()),
    }
}
