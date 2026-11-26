//! The errors a parse, or a placement, reports. Each collects them, so that it reports every
//! rule it checks that the IR breaks, not only the first.
//!
//! A part that fails to parse returns a [`Reported`], which only [`Errors::add`] makes, so a
//! failed parse always has an error to return.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::error::Error;
use std::fmt::{self, Write as _};

use crate::ir::{JsonPointer, Name};

/// A rule an IR breaks: what is wrong, where, why, and each change that would fix it.
///
/// It reads one fact a line, as rustc's diagnostics do. `error:` starts what is wrong, `-->` where
/// it is, and `=` each why. `= help:` starts the changes, any one of which fixes it.
/// [`Display`](fmt::Display) gives the lines plain, and [`ParseError::styled`] styles them.
// rustc's `Diagnostic` holds the same parts, and its emitter lays them out so:
// https://github.com/rust-lang/rust/blob/897e37553bba8b42751c67658967889d11ecd120/compiler/rustc_errors/src/diagnostic.rs#L194-L232
// https://github.com/rust-lang/rust/blob/897e37553bba8b42751c67658967889d11ecd120/compiler/rustc_errors/src/emitter.rs#L1339-L1344
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub at: At,
    /// What is wrong, such as that a role fits no class of sandbox.
    pub message: String,
    /// What the IR holds at `at`, as `message` reads it, such as a role's needs.
    pub label: Option<String>,
    /// Each fact that makes it wrong, such as what keeps a class from fitting a role.
    pub why: Vec<String>,
    /// Each change that would fix it.
    pub help: Vec<String>,
}

/// Where a [`ParseError`] is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct At {
    /// Where in the IR, such as `/roles/0/runtime/agent/grants/0/job/target`. It resolves in the
    /// JSON that the [`ir::Workflow`](crate::ir::Workflow) parsed serializes to.
    pub ir: JsonPointer,
    /// Where in the source that the IR was written in, such as a task's entry file, once
    /// [`ParseErrors::in_source`] gives it.
    pub source: Option<String>,
}

/// A part of a [`ParseError`]'s lines that [`ParseError::styled`] styles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// `error:`, which starts the first line.
    Error,
    /// What is wrong, which ends the first line.
    Message,
    /// The `-->` or `=` that starts a later line.
    Margin,
    /// `help:`, which starts the changes that would fix it.
    Help,
}

impl ParseError {
    /// Its lines, as [`Display`](fmt::Display) gives them, with each [`Part`] as `style` gives
    /// that part's text.
    pub fn styled(&self, style: impl Fn(Part, &str) -> String) -> String {
        let Self {
            at: At { ir, source },
            message,
            label,
            why,
            help,
        } = self;
        let error = style(Part::Error, "error:");
        let mut lines = Vec::from([format!("{error} {}", style(Part::Message, message))]);
        let place = [source.as_deref(), Some(&path(ir)), label.as_deref()];
        let place: Vec<&str> = place.into_iter().flatten().collect();
        lines.push(format!(
            "  {} {}",
            style(Part::Margin, "-->"),
            place.join(": ")
        ));
        let equals = style(Part::Margin, "=");
        lines.extend(why.iter().map(|why| format!("  {equals} {why}")));
        let helped = format!("  {equals} {} ", style(Part::Help, "help:"));
        // Each change after the first lines up with it, past `= help: `.
        let indent = " ".repeat("  = help: ".len());
        for (index, change) in help.iter().enumerate() {
            let comma = if index + 1 < help.len() { "," } else { "" };
            lines.push(match index {
                0 => format!("{helped}{change}{comma}"),
                _ => format!("{indent}or {change}{comma}"),
            });
        }
        lines.join("\n")
    }
}

/// `pointer` as a person reads a path, such as `roles[2].needs`: each index in brackets, and each
/// key after a dot. A key that holds more than letters, digits, `_` and `-` is in brackets, as a
/// JSON string. The whole IR reads as `the workflow`.
// A key of the IR is a field's name or a `Name`, which starts with a letter. So a token of digits
// alone is an index.
fn path(pointer: &JsonPointer) -> String {
    let mut path = String::new();
    for token in pointer.tokens() {
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');
        if !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()) {
            let _ = write!(path, "[{token}]");
        } else if token
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
            && token.bytes().all(plain)
        {
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(&token);
        } else {
            let _ = write!(path, "[{}]", serde_json::Value::String(token));
        }
    }
    if path.is_empty() {
        "the workflow".to_owned()
    } else {
        path
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.styled(|_, text| text.to_owned()))
    }
}

impl Error for ParseError {}

/// The rules an IR breaks, in the order the parse, or the placement, finds them; see
/// [`Workflow::parse`](crate::workflow::Workflow::parse) and
/// [`Workflow::place`](crate::workflow::Workflow::place) for which rules. Never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseErrors(Vec<ParseError>);

impl ParseErrors {
    pub fn as_slice(&self) -> &[ParseError] {
        &self.0
    }

    /// These errors, each [at](At::source) the place in the source that `source` gives for its
    /// place in the IR.
    #[must_use]
    pub fn in_source(mut self, source: impl Fn(&JsonPointer) -> String) -> Self {
        for error in &mut self.0 {
            error.at.source = Some(source(&error.at.ir));
        }
        self
    }
}

impl fmt::Display for ParseErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut errors = self.0.iter();
        if let Some(first) = errors.next() {
            write!(f, "{first}")?;
        }
        for error in errors {
            write!(f, "\n{error}")?;
        }
        Ok(())
    }
}

impl Error for ParseErrors {}

/// Shows that an error has been reported.
#[derive(Clone, Copy, Debug)]
pub(super) struct Reported(());

#[derive(Default)]
pub(super) struct Errors(Vec<ParseError>);

impl Errors {
    /// Reports that the IR at `at` breaks a rule, which `message` says whole.
    pub(super) fn report(&mut self, at: JsonPointer, message: impl Into<String>) -> Reported {
        self.add(ParseError {
            at: At {
                ir: at,
                source: None,
            },
            message: message.into(),
            label: None,
            why: Vec::new(),
            help: Vec::new(),
        })
    }

    pub(super) fn add(&mut self, error: ParseError) -> Reported {
        self.0.push(error);
        Reported(())
    }

    /// Ends a parse, or a placement: fails with the errors reported, if there are any.
    pub(super) fn finish<T>(self, parsed: Result<T, Reported>) -> Result<T, ParseErrors> {
        match parsed {
            Ok(parsed) if self.0.is_empty() => Ok(parsed),
            Ok(_) | Err(Reported(())) => Err(ParseErrors(self.0)),
        }
    }

    /// Reports each of a list's `names` that an earlier one repeats. The part that holds the
    /// name at an index, as its `name`, is at `at` of that index.
    pub(super) fn unique<'n>(
        &mut self,
        names: impl IntoIterator<Item = &'n Name>,
        at: impl Fn(usize) -> JsonPointer,
    ) {
        let mut first: BTreeMap<&Name, usize> = BTreeMap::new();
        for (index, name) in names.into_iter().enumerate() {
            match first.entry(name) {
                Entry::Vacant(entry) => {
                    entry.insert(index);
                }
                Entry::Occupied(entry) => {
                    let earlier = at(*entry.get());
                    let message = format!("`{name}` also names {earlier}: give each a unique name");
                    self.report(at(index).token("name"), message);
                }
            }
        }
    }
}
