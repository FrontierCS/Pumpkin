//! A call's arguments, evaluated, with the keyword ones read in one place, [`Call::keywords`],
//! so that a reader takes those that `**` passes with those written by name.

use alloc::format;
use alloc::vec::Vec;

use crate::builtins::origin;
use crate::problem::Fault;
use crate::syntax::Name;
use crate::value::{Budget, Key, Kind, Span, Text, Value};

// A call's arguments, evaluated.
pub(crate) struct Call<'a> {
    /// The whole call, where a fault of the call points.
    pub(crate) at: Span,
    pub(crate) positional: Vec<(Span, Value)>,
    keyword: Vec<(&'a Name, Value)>,
    kwargs: Option<Kwargs>,
    pub(crate) budget: &'a Budget,
}

// The entries of the dict that `**` passes, each keyed by a string that names no keyword argument
// written by name, in the dict's order.
struct Kwargs {
    /// Where the call writes the dict, where each of its keyword arguments is given.
    at: Span,
    entries: Vec<(Text, Value)>,
}

// A keyword argument: its name, where it is given and its value.
pub(crate) struct Keyword<'c> {
    pub(crate) name: &'c str,
    pub(crate) span: Span,
    pub(crate) value: &'c Value,
}

impl<'a> Call<'a> {
    /// The call of `positional` and `keyword` arguments, then of the entries of `kwargs`, the
    /// dict that `**` passes, if any, and where the call writes it.
    ///
    /// # Errors
    ///
    /// `kwargs` is no dict, one of its keys is no string, or one names a keyword argument of
    /// `keyword`.
    pub(crate) fn new(
        at: Span,
        positional: Vec<(Span, Value)>,
        keyword: Vec<(&'a Name, Value)>,
        kwargs: Option<(Span, Value)>,
        budget: &'a Budget,
    ) -> Result<Self, Fault> {
        let kwargs = match kwargs {
            Some((at, value)) => Some(Kwargs {
                at,
                entries: unpacked(&value, at, &keyword)?,
            }),
            None => None,
        };
        Ok(Self {
            at,
            positional,
            keyword,
            kwargs,
            budget,
        })
    }

    /// Each keyword argument: those written by name, then those that `**` passes.
    pub(crate) fn keywords(&self) -> impl Iterator<Item = Keyword<'_>> {
        let written = self.keyword.iter().map(|(name, value)| Keyword {
            name: &name.text,
            span: name.span,
            value,
        });
        let passed = self.kwargs.iter().flat_map(|kwargs| {
            (kwargs.entries.iter()).map(|(name, value)| Keyword {
                name,
                span: kwargs.at,
                value,
            })
        });
        written.chain(passed)
    }
}

// The entries of `value`, the dict that `**` passes at `at`. An entry keyed like one of `named`,
// the keywords given by name, is refused.
fn unpacked(
    value: &Value,
    at: Span,
    named: &[(&Name, Value)],
) -> Result<Vec<(Text, Value)>, Fault> {
    let Kind::Dict(dict) = &value.kind else {
        let message = format!(
            "{} can't be passed with `**`: pass a dict of keyword arguments",
            value.kind.describe()
        );
        return Err(origin(Fault::new(at, message), "the value", value));
    };
    let mut entries = Vec::with_capacity(dict.len());
    for entry in dict.iter() {
        let Key::Str(text) = &entry.key else {
            let message = format!(
                "`{}`, a key of the dict passed with `**`, is no string: key each keyword \
                 argument by its name",
                entry.key.literal()
            );
            return Err(origin(
                Fault::new(at, message),
                "the key",
                &entry.key_value(),
            ));
        };
        if let Some((first, _)) = named.iter().find(|(name, _)| name.text == **text) {
            let message = format!(
                "`{}` is given by name already, and again with `**`: give it once",
                first.text
            );
            return Err(Fault::new(at, message).note("it is first given at", Some(first.span)));
        }
        entries.push((text.clone(), entry.value.clone()));
    }
    Ok(entries)
}
