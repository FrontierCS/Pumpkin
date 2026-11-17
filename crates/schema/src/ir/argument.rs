//! Arguments given as text, as `--arg <name>=<value>`, each read against its type before what
//! takes it runs: a workflow's, and a body's caller's, whose types the IR records.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use serde::{Deserialize, Serialize};

use super::Name;

/// A type that an argument given as text is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentType {
    Str,
    Int,
    Bool,
}

/// An argument's value, of its [`ArgumentType`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgumentValue {
    Str(String),
    Int(i64),
    Bool(bool),
}

/// A parameter of a body that its caller gives: its type, with its default if it has one.
///
/// Written as its type and its default, `{"type": "bool", "default": false}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Argument {
    Str {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    Int {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
    },
    Bool {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
}

impl ArgumentType {
    /// The value of `--arg <name>=<text>`, read as this type: a string as it is, an int in
    /// decimal digits, a bool as `true` or `false`.
    ///
    /// # Errors
    ///
    /// What is wrong with `text`, and what this type reads.
    pub fn read(self, name: &str, text: &str) -> Result<ArgumentValue, String> {
        match self {
            Self::Str => Ok(ArgumentValue::Str(text.to_owned())),
            Self::Int => text.parse::<i64>().map(ArgumentValue::Int).map_err(|_| {
                format!(
                    "`--arg {name}` is `{text:?}`, which `int` can't read: give it decimal digits, \
                     after `-` or `+` or neither, from -2^63 to 2^63 - 1"
                )
            }),
            Self::Bool => match text {
                "true" => Ok(ArgumentValue::Bool(true)),
                "false" => Ok(ArgumentValue::Bool(false)),
                _ => Err(format!(
                    "`--arg {name}` is `{text:?}`, which is no bool: give it `true` or `false`"
                )),
            },
        }
    }
}

/// The texts that `given` writes, each as `<name>=<value>`, by name: a name is its text before its
/// first `=`, and its value the rest, whatever it holds.
///
/// # Errors
///
/// What is wrong with each one that has no `=`, has no name, or names one given before it.
pub fn texts<'g>(
    given: impl IntoIterator<Item = &'g str>,
) -> Result<BTreeMap<String, String>, Vec<String>> {
    let mut texts = BTreeMap::new();
    let mut problems = Vec::new();
    for text in given {
        let problem = match text.split_once('=') {
            None => "has no `=`: give it as `--arg <name>=<value>`",
            Some(("", _)) => "names no arg before its `=`: give it as `--arg <name>=<value>`",
            Some((name, value)) => match texts.entry(name.to_owned()) {
                Entry::Vacant(slot) => {
                    slot.insert(value.to_owned());
                    continue;
                }
                Entry::Occupied(_) => "names an arg given already: give each arg once",
            },
        };
        problems.push(format!("`--arg {}` {problem}", text.escape_debug()));
    }
    if problems.is_empty() {
        Ok(texts)
    } else {
        Err(problems)
    }
}

/// The values of `given`, the texts that `function`'s caller gives by name, each read as its
/// parameter among `parameters` is typed, and each parameter that `given` misses at its default.
///
/// # Errors
///
/// The first text that names no parameter or doesn't read, or the first parameter without a
/// default that `given` misses.
pub fn read(
    function: &Name,
    parameters: &BTreeMap<Name, Argument>,
    given: &BTreeMap<Name, String>,
) -> Result<BTreeMap<Name, ArgumentValue>, String> {
    if let Some(name) = given.keys().find(|name| !parameters.contains_key(*name)) {
        return Err(format!(
            "`--arg {name}` is given, and `{function}` takes no such argument: give only those \
             that `ops` lists"
        ));
    }
    let mut values = BTreeMap::new();
    for (name, parameter) in parameters {
        let (of_type, default) = match parameter {
            Argument::Str { default } => {
                (ArgumentType::Str, default.clone().map(ArgumentValue::Str))
            }
            Argument::Int { default } => (ArgumentType::Int, default.map(ArgumentValue::Int)),
            Argument::Bool { default } => (ArgumentType::Bool, default.map(ArgumentValue::Bool)),
        };
        let value = match (given.get(name), default) {
            (Some(text), _) => of_type.read(name.as_str(), text)?,
            (None, Some(default)) => default,
            (None, None) => {
                return Err(format!(
                    "`--arg {name}` is required, as `{function}` gives it no default: give it as \
                     `--arg {name}=<value>`"
                ));
            }
        };
        values.insert(name.clone(), value);
    }
    Ok(values)
}
