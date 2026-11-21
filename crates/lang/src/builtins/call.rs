//! A call's method and arguments. The receiver binds its method before the arguments run, and
//! [`Call::keywords`] reads the keywords passed by name and by `**` together.

use alloc::format;
use alloc::vec::Vec;

use super::{DictMethod, ModuleMethod, StringMethod, origin};
use crate::Loader;
use crate::eval::{Evaluation, body};
use crate::problem::Fault;
use crate::syntax::Name;
use crate::value::{Budget, Dict, Key, Kind, Span, Text, Value};

pub(crate) enum BoundMethod<'v> {
    Dict(&'v Dict, DictMethod),
    String(&'v str, &'v (Span, Value), StringMethod),
    Module(ModuleMethod),
    Hosted(crate::value::Hosted<usize, u64>, body::Method),
}

impl BoundMethod<'_> {
    pub(crate) fn call<L: Loader>(
        self,
        name: &Name,
        call: &Call<'_>,
        evaluation: &mut Evaluation<'_, '_, L>,
        depth: usize,
    ) -> Result<Value, Fault> {
        match self {
            Self::Dict(dict, method) => method(&name.text, dict, call),
            Self::String(text, receiver, method) => Ok(Value {
                kind: method(&name.text, text, receiver, call)?,
                span: call.at,
            }),
            Self::Module(method) => method(call),
            Self::Hosted(handle, method) => method.call(evaluation, handle, name, call, depth),
        }
    }
}

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

    /// The call of `positional` and `keyword` arguments alone.
    pub(crate) fn of(
        at: Span,
        positional: Vec<(Span, Value)>,
        keyword: Vec<(&'a Name, Value)>,
        budget: &'a Budget,
    ) -> Self {
        Self {
            at,
            positional,
            keyword,
            kwargs: None,
            budget,
        }
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
