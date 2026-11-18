//! Bounded SSE records. A record's data lines join with newlines; its other fields carry
//! no model output. An oversized record is skipped through its blank line.
//!
//! <https://html.spec.whatwg.org/multipage/server-sent-events.html#event-stream-interpretation>.

use crate::usage::MAX_HELD_BYTES;

#[derive(Default)]
pub(crate) struct Frames {
    offset: usize,
    line_start: usize,
    after_cr: bool,
    line: Vec<u8>,
    data: Vec<u8>,
    data_start: usize,
    error_event: bool,
    skipping: bool,
}

pub(crate) struct Frame {
    pub(crate) data: Vec<u8>,
    pub(crate) error_event: bool,
    /// The start of the first data line and the offset at which the blank line dispatched it.
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl Frames {
    pub(crate) fn pending(&self) -> bool {
        self.skipping || !self.line.is_empty() || !self.data.is_empty()
    }

    /// Advances `input` to the next complete record. An oversized record returns an error once;
    /// later calls consume what remains of it and return subsequent records.
    pub(crate) fn next(&mut self, input: &mut &[u8]) -> Result<Option<Frame>, &'static str> {
        while !input.is_empty() {
            if std::mem::take(&mut self.after_cr)
                && let Some(rest) = input.strip_prefix(b"\n")
            {
                self.offset += 1;
                self.line_start = self.offset;
                *input = rest;
                continue;
            }
            let end = input.iter().position(|b| matches!(b, b'\n' | b'\r'));
            let (piece, rest) = input.split_at(end.unwrap_or(input.len()));
            self.offset += piece.len();
            let length = self.offset - self.line_start;
            let oversized = !self.skipping && length > MAX_HELD_BYTES;
            if oversized {
                self.discard();
            }
            if !self.skipping {
                self.line.extend_from_slice(piece);
            }
            let Some((&delimiter, rest)) = rest.split_first() else {
                *input = rest;
                return if oversized {
                    Err("SSE line exceeds the route's byte limit")
                } else {
                    Ok(None)
                };
            };
            *input = rest;
            self.after_cr = delimiter == b'\r';
            self.offset += 1;
            let start = std::mem::replace(&mut self.line_start, self.offset);
            if oversized {
                return Err("SSE line exceeds the route's byte limit");
            }
            if self.skipping {
                self.skipping = length != 0;
                continue;
            }
            let raw = std::mem::take(&mut self.line);
            let line = if start == 0 {
                raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw)
            } else {
                &raw
            };
            if let Some(frame) = self.field(start, line)? {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    fn field(&mut self, start: usize, line: &[u8]) -> Result<Option<Frame>, &'static str> {
        if line.is_empty() {
            let error_event = std::mem::take(&mut self.error_event);
            if self.data.pop().is_some() {
                return Ok(Some(Frame {
                    data: std::mem::take(&mut self.data),
                    error_event,
                    start: self.data_start,
                    end: self.offset,
                }));
            }
            return Ok(None);
        }
        let (field, value) =
            line.iter()
                .position(|&byte| byte == b':')
                .map_or((line, &[][..]), |at| {
                    let value = &line[at + 1..];
                    (&line[..at], value.strip_prefix(b" ").unwrap_or(value))
                });
        match field {
            b"data" => {
                if value.len() >= MAX_HELD_BYTES - self.data.len() {
                    self.discard();
                    return Err("SSE data exceeds the route's byte limit");
                }
                if self.data.is_empty() {
                    self.data_start = start;
                }
                self.data.extend_from_slice(value);
                self.data.push(b'\n');
            }
            b"event" => self.error_event = value == b"error",
            _ => {}
        }
        Ok(None)
    }

    fn discard(&mut self) {
        self.data = Vec::new();
        self.line = Vec::new();
        self.error_event = false;
        self.skipping = true;
    }
}
