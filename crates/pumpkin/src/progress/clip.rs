//! Text cut after a count of characters, as `…` ends it then, for the words that show what a
//! run's record holds whole.

use std::fmt;

/// `text`, cut after `chars`.
pub(super) fn clipped(text: &str, chars: usize) -> String {
    let mut clip = Clip::new(chars);
    let _ = fmt::Write::write_str(&mut clip, text);
    clip.text()
}

/// What is written to it, cut after a count of characters. The write that passes the count, and
/// each after, fails, so that a formatter stops there rather than format what is cut.
pub(super) struct Clip {
    text: String,
    left: usize,
    cut: bool,
}

impl Clip {
    pub(super) const fn new(chars: usize) -> Self {
        Self {
            text: String::new(),
            left: chars,
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
        let Some((at, _)) = text.char_indices().nth(self.left) else {
            self.text.push_str(text);
            self.left -= text.chars().count();
            return Ok(());
        };
        self.text.push_str(&text[..at]);
        self.text.push('…');
        self.cut = true;
        Err(fmt::Error)
    }
}
