//! How `pumpkin` styles what it prints: in cargo's palette, on a stream that anstream finds
//! styled, and plain on any other.

use anstream::stream::RawStream;
use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use schema::workflow::{ParseError, Part};

pub(crate) const HEAD: Style = Style::new().bold();
pub(crate) const DIM: Style = Style::new().dimmed();
// Cargo's palette: green for what went well, yellow for a warning, red for an error.
// https://github.com/rust-lang/cargo/blob/111443f3dc4d82a65e2306a93028f122406401c1/crates/cargo-util-terminal/src/style.rs#L4-L15
pub(crate) const LIVE: Style = AnsiColor::Cyan.on_default();
pub(crate) const GOOD: Style = AnsiColor::Green.on_default();
pub(crate) const WARN: Style = AnsiColor::Yellow.on_default();
pub(crate) const BAD: Style = AnsiColor::Red.on_default().bold();

/// Whether what goes to `stream` is styled, as anstream decides it for clap's help.
pub(crate) fn styled(stream: &impl RawStream) -> bool {
    match AutoStream::choice(stream) {
        ColorChoice::Never => false,
        ColorChoice::Auto | ColorChoice::AlwaysAnsi | ColorChoice::Always => true,
    }
}

/// Styles text where the lines are styled, and leaves it plain where they aren't.
pub(crate) struct Paint(pub(crate) bool);

impl Paint {
    pub(crate) fn on(&self, style: Style, text: &str) -> String {
        if self.0 && !text.is_empty() {
            format!("{style}{text}{style:#}")
        } else {
            text.to_owned()
        }
    }

    /// `error`'s lines, styled in the palette.
    pub(crate) fn error(&self, error: &ParseError) -> String {
        // As rustc shows them: `error` red, what is wrong bold, and `help` cyan. Its margin is
        // blue, which the palette lacks, so it is dimmed here.
        // https://github.com/rust-lang/rust/blob/897e37553bba8b42751c67658967889d11ecd120/compiler/rustc_errors/src/lib.rs#L1586-L1605
        error.styled(|part, text| {
            let style = match part {
                Part::Error => BAD,
                Part::Message => HEAD,
                Part::Margin => DIM,
                Part::Help => LIVE,
            };
            self.on(style, text)
        })
    }
}
