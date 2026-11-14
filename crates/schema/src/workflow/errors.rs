//! The errors a parse, or a placement, reports. Each collects them, so that it reports every
//! rule it checks that the IR breaks, not only the first.
//!
//! A part that fails to parse returns a [`Reported`], which only [`Errors::report`] makes, so a
//! failed parse always has an error to return.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::error::Error;
use std::fmt;

use crate::ir::{JsonPointer, Name};

/// A rule an IR breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Where in the IR, such as `/roles/0/grants/0/judge/role`. It resolves in the JSON that
    /// the [`ir::Workflow`](crate::ir::Workflow) parsed serializes to.
    pub at: JsonPointer,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.at, self.message)
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
    pub(super) fn report(&mut self, at: JsonPointer, message: impl Into<String>) -> Reported {
        self.0.push(ParseError {
            at,
            message: message.into(),
        });
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
