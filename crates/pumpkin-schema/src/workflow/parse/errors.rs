//! The errors a parse reports. A parse collects them, so that it reports every rule it checks
//! that the IR breaks, not only the first.
//!
//! A part that fails to parse returns a [`Reported`], which only [`Errors::report`] makes, so a
//! failed parse always has an error to return.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::ir::Name;

/// A rule an IR breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Where in the IR, such as `main.solo_then_judge.grants[train].judge.role`. A part is
    /// named by its name, or by its index where its name repeats, such as `roles[3].name`.
    pub path: String,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl Error for ParseError {}

/// The rules an IR breaks, in the order the parse finds them; see
/// [`Workflow::parse`](crate::workflow::Workflow::parse) for which rules. Never empty.
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
    pub(super) fn report(
        &mut self,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Reported {
        self.0.push(ParseError {
            path: path.into(),
            message: message.into(),
        });
        Reported(())
    }

    /// Ends a parse: fails with the errors reported, if there are any.
    pub(super) fn finish<T>(self, parsed: Result<T, Reported>) -> Result<T, ParseErrors> {
        match parsed {
            Ok(parsed) if self.0.is_empty() => Ok(parsed),
            Ok(_) | Err(Reported(())) => Err(ParseErrors(self.0)),
        }
    }

    /// Labels the parts of a collection for their error paths, as [`ParseError::path`] names
    /// them, and reports each name that repeats an earlier one.
    pub(super) fn unique<'n>(
        &mut self,
        collection: &str,
        names: impl IntoIterator<Item = &'n Name>,
    ) -> Vec<String> {
        let names: Vec<&Name> = names.into_iter().collect();
        // The index of each name's first use, and how many parts use it.
        let mut uses: HashMap<&Name, (usize, usize)> = HashMap::new();
        for (index, name) in names.iter().enumerate() {
            uses.entry(name).or_insert((index, 0)).1 += 1;
        }
        let mut labels = Vec::with_capacity(names.len());
        for (index, name) in names.into_iter().enumerate() {
            let (first, count) = uses[name];
            if count == 1 {
                labels.push(format!("{collection}[{name}]"));
                continue;
            }
            let label = format!("{collection}[{index}]");
            if index != first {
                self.report(
                    format!("{label}.name"),
                    format!("`{name}` also names {collection}[{first}]: give each a unique name"),
                );
            }
            labels.push(label);
        }
        labels
    }

    /// Resolves one reference, reporting an unknown name.
    pub(super) fn resolve<Id: Copy>(
        &mut self,
        path: &str,
        name: &Name,
        ids: &HashMap<&str, Id>,
        kind: &str,
    ) -> Result<Id, Reported> {
        ids.get(name.as_str()).copied().ok_or_else(|| {
            self.report(
                path,
                format!("no {kind} is named `{name}`: name one listed in `{kind}s`"),
            )
        })
    }
}
