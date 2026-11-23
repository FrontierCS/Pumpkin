//! Text cut to at most a count of bytes, at a character's start, as `…` ends it then, for the
//! words that show what a run's record holds whole.

use std::fmt;

/// `shown` as text, cut to at most `bytes`. Its formatting stops at the cut.
pub(crate) fn clipped(shown: impl fmt::Display, bytes: usize) -> String {
    let mut clip = Clip::new(bytes);
    let _ = fmt::Write::write_fmt(&mut clip, format_args!("{shown}"));
    clip.text()
}

/// What is written to it, cut to at most a count of bytes, at a character's start. The write that
/// passes the count, and each after, fails, so that a formatter stops there rather than format what
/// is cut.
pub(super) struct Clip {
    text: String,
    left: usize,
    cut: bool,
}

impl Clip {
    pub(super) const fn new(bytes: usize) -> Self {
        Self {
            text: String::new(),
            left: bytes,
            cut: false,
        }
    }

    pub(super) fn text(self) -> String {
        self.text
    }
}

impl fmt::Write for Clip {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.cut {
            return Err(fmt::Error);
        }
        if text.len() <= self.left {
            self.text.push_str(text);
            self.left -= text.len();
            return Ok(());
        }
        self.text
            .push_str(&text[..text.floor_char_boundary(self.left)]);
        self.text.push('…');
        self.cut = true;
        Err(fmt::Error)
    }
}
