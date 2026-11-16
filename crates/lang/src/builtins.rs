//! The predeclared names (spec.md, "Built-in constants and functions" and "Built-in methods"),
//! Pumpkin's own among them.
//!
//! Each does what the specification has it do, but for what the subset refuses:
//! - `str`, `fail` and `format` format ints and strings only;
//! - `len` takes no string, as Starlark's implementations count its elements differently;
//! - `int` reads a string of decimal digits, after a sign or none, and takes no base;
//! - `sorted` sorts a list, a tuple or a dict's keys, all ints or all strings, and takes no `key`
//!   or `reverse`;
//! - `bool` takes a value, `fail` one, its message, and a `format` field is `{}`, `{0}` or
//!   `{name}`, with no conversion or format spec;
//! - `split` takes a separator, `startswith` and `endswith` one string, and `replace` no count,
//!   nor an empty string to replace, which Starlark's implementations place differently;
//! - a dict's keys are strings or ints, as `crate::value::Key` has them, and no method changes a
//!   value, as none does in the subset.
//!
//! Each takes its arguments by position, but `format`, which takes keyword ones too.
//!
//! `json` is the module of Bazel's and starlark-go's Starlark, with its functions `encode` and
//! `decode`, as [`crate::json`] has them, and without `encode_indent` and `indent`.
//!
//! What the subset leaves out is refused where it is named, with what to write instead, not told
//! to be unknown: a function by [`left_out`], a method by the lists of those it leaves out.

mod call;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::fmt;
use core::num::IntErrorKind;

pub(crate) use call::{Call, Keyword};

use crate::problem::{self, Fault, list, refusal};
use crate::syntax::{Def, Name, is_name};
use crate::value::{
    Budget, Builtin, Dict, Element, Entry, Key, Kind, Module, Nested, Span, Text, Value, escape,
};
use crate::{json, pumpkin};

/// The value that `name` names in every file that doesn't bind it.
pub(crate) fn predeclared(name: &str) -> Option<Kind> {
    match name {
        "None" => Some(Kind::None),
        "True" => Some(Kind::Bool(true)),
        "False" => Some(Kind::Bool(false)),
        "json" => Some(Kind::Module(Module::Json)),
        _ => match (Universal::ALL.into_iter()).find(|universal| universal.name() == name) {
            Some(universal) => Some(Kind::Builtin(Builtin::Universal(universal))),
            None => pumpkin::predeclared(name),
        },
    }
}

// The refusal of the name of a Starlark function that the subset leaves out, if it is one.
pub(crate) fn left_out(name: &str) -> Option<String> {
    let (why, instead) = match name {
        "print" => (" whose output nothing reads", "show a value with `fail`"),
        "dict" | "enumerate" | "list" | "range" | "reversed" | "set" | "tuple" | "zip" => {
            ("", "write the value out, or build it with a comprehension")
        }
        "abs" | "all" | "any" | "bytes" | "dir" | "float" | "getattr" | "hasattr" | "hash"
        | "max" | "min" | "repr" | "type" => (
            "",
            "compute the value with `bool`, `int`, `len`, `sorted` and `str`",
        ),
        _ => return None,
    };
    Some(refusal(
        format_args!("`{name}`, a builtin of Starlark's{why},"),
        instead,
    ))
}

/// Each of `params`, in order, with the argument that `call` gives it by position or by name, or
/// `None` where it gives none. Only the first `positional` of `params` may be given by position.
///
/// # Errors
///
/// More arguments by position than `positional`, a keyword that names none of `params`, or a
/// parameter given twice.
pub(crate) fn place(
    function: &str,
    params: &[&str],
    positional: usize,
    call: &Call<'_>,
) -> Result<Vec<Option<Value>>, Fault> {
    if let Some((span, _)) = call.positional.get(positional) {
        return Err(surplus(function, params, positional, *span));
    }
    let mut given: Vec<Option<Value>> = vec![None; params.len()];
    for (place, (_, value)) in call.positional.iter().enumerate() {
        given[place] = Some(value.clone());
    }
    for keyword in call.keywords() {
        let Some(place) = params.iter().position(|param| *param == keyword.name) else {
            let message = if params.is_empty() {
                format!(
                    "`{}` is no parameter of `{function}`, which takes no argument: drop it",
                    keyword.name
                )
            } else {
                format!(
                    "`{}` is no parameter of `{function}`: expected one of {}",
                    keyword.name,
                    list(params)
                )
            };
            return Err(Fault::new(keyword.span, message));
        };
        if given[place].is_some() {
            let message = format!(
                "`{}` is given by position already: give it once",
                keyword.name
            );
            return Err(Fault::new(keyword.span, message));
        }
        given[place] = Some(keyword.value.clone());
    }
    Ok(given)
}

pub(crate) fn lacks(function: &str, missing: &[&str], at: Span) -> Fault {
    let them = if missing.len() == 1 { "it" } else { "them" };
    let message = format!("`{function}` lacks {}: give {them}", list(missing));
    Fault::new(at, message)
}

fn surplus(function: &str, params: &[&str], positional: usize, at: Span) -> Fault {
    let message = match (positional, params.first()) {
        (_, None) => format!("`{function}` takes no argument: drop it"),
        (0, Some(first)) => {
            format!("`{function}` takes its arguments by name only: name each, as `{first} = …`")
        }
        (positional, Some(_)) => format!(
            "`{function}` takes {} by position: name the rest",
            arguments(positional)
        ),
    };
    Fault::new(at, message)
}

pub(crate) fn called(def: &Def) -> String {
    format!("in `{}()`, called at", def.name.text)
}

// The functions in scope in every file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Universal {
    Bool,
    Fail,
    Int,
    Len,
    Sorted,
    Str,
}

impl Universal {
    const ALL: [Self; 6] = [
        Self::Bool,
        Self::Fail,
        Self::Int,
        Self::Len,
        Self::Sorted,
        Self::Str,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Fail => "fail",
            Self::Int => "int",
            Self::Len => "len",
            Self::Sorted => "sorted",
            Self::Str => "str",
        }
    }

    fn takes(self) -> Takes {
        match self {
            Self::Bool => Takes {
                min: 0,
                max: 1,
                keywords: &[],
            },
            Self::Fail => Takes {
                min: 0,
                max: usize::MAX,
                keywords: &["msg", "attr", "sep", "stack_trace"],
            },
            Self::Int => Takes {
                min: 0,
                max: 2,
                keywords: &["x", "base"],
            },
            Self::Sorted => Takes {
                min: 1,
                max: 3,
                keywords: &["iterable", "key", "reverse"],
            },
            Self::Len | Self::Str => Takes::exactly(1),
        }
    }

    pub(crate) fn call(self, call: &Call<'_>) -> Result<Value, Fault> {
        let name = self.name();
        let [argument] = positional::<1>(name, self.takes(), call)?;
        let (_, value) = argument;
        let kind = match self {
            Self::Bool => Kind::Bool(value.truth()),
            Self::Fail => {
                let message = format!("fail: {}", text(name, argument)?);
                return Err(Fault::new(call.at, message));
            }
            Self::Int => Kind::Int(int(argument)?),
            Self::Len => match &value.kind {
                Kind::List(items) | Kind::Tuple(items) => Kind::Int(count(items.len())),
                Kind::Dict(dict) => Kind::Int(count(dict.len())),
                Kind::Str(_) => {
                    let message = refusal(
                        "`len` of a string, whose elements Starlark's implementations count \
                         differently,",
                        "give it a list, a tuple or a dict",
                    );
                    return Err(origin(
                        Fault::new(argument.0, message),
                        "the argument",
                        value,
                    ));
                }
                Kind::None
                | Kind::Bool(_)
                | Kind::Int(_)
                | Kind::Function(_)
                | Kind::Builtin(_)
                | Kind::Image(_)
                | Kind::Module(_) => {
                    return Err(rejected(name, argument, "a list, a tuple or a dict"));
                }
            },
            Self::Sorted => Kind::List(sorted(argument, call.budget)?),
            Self::Str => Kind::Str(Text::new(&text(name, argument)?, call.at, call.budget)?),
        };
        Ok(Value {
            kind,
            span: call.at,
        })
    }
}

fn count(n: usize) -> i64 {
    let Ok(n) = i64::try_from(n) else {
        unreachable!(
            "a length of {n} is over 2^63 - 1, though a length counts elements in memory, which \
             are fewer than 2^63"
        )
    };
    n
}

// What Starlark lets a call of a builtin give, in any of its implementations: from `min` to `max`
// arguments by position, and by keyword those `keywords` name. The subset takes the arguments it
// needs by position only, so a call that Starlark accepts and the subset does not is refused, and
// any other call is Starlark's own error.
#[derive(Clone, Copy)]
struct Takes {
    min: usize,
    max: usize,
    keywords: &'static [&'static str],
}

impl Takes {
    const fn exactly(n: usize) -> Self {
        Self {
            min: n,
            max: n,
            keywords: &[],
        }
    }
}

// The `N` arguments by position that `function` takes, or the fault of a call that gives other.
fn positional<'c, const N: usize>(
    function: &str,
    starlark: Takes,
    call: &'c Call<'_>,
) -> Result<&'c [(Span, Value); N], Fault> {
    if let Some(keyword) = call.keywords().next() {
        let takes = arguments(N);
        let message = if starlark.keywords.contains(&keyword.name) {
            refusal(
                format_args!("`{function}`'s `{}`", keyword.name),
                format_args!("give `{function}` {takes}, by position"),
            )
        } else {
            format!(
                "`{function}` takes no keyword argument `{}`: give it {takes}, by position",
                keyword.name
            )
        };
        return Err(Fault::new(keyword.span, message));
    }
    (call.positional.as_slice().try_into()).map_err(|_| {
        let (given, takes) = (call.positional.len(), arguments(N));
        let message = if (starlark.min..=starlark.max).contains(&given) {
            refusal(
                format_args!("`{function}` with {}", arguments(given)),
                format_args!("give it {takes}"),
            )
        } else {
            format!("`{function}` takes {takes}, and is given {given}: give it {takes}")
        };
        Fault::new(call.at, message)
    })
}

fn arguments(n: usize) -> String {
    match n {
        0 => String::from("no argument"),
        n => problem::count(n, "argument"),
    }
}

// The error of `argument`, which Starlark's `function` rejects too: it takes `takes`.
fn rejected(function: &str, argument: &(Span, Value), takes: &str) -> Fault {
    let (at, value) = argument;
    let message = format!(
        "`{function}` can't take {}: give it {takes}",
        value.kind.describe()
    );
    origin(Fault::new(*at, message), "the argument", value)
}

// The refusal of `argument`, which Starlark's `function` takes, and the subset's takes `takes`
// instead.
fn narrowed(function: &str, argument: &(Span, Value), takes: &str) -> Fault {
    let (at, value) = argument;
    let message = refusal(
        format_args!("`{function}` of {}", value.kind.describe()),
        format_args!("give it {takes}"),
    );
    origin(Fault::new(*at, message), "the argument", value)
}

/// `fault`, with a note on where `value`, which `what` names, was made, unless the fault is
/// already there.
pub(crate) fn origin(fault: Fault, what: &str, value: &Value) -> Fault {
    if value.span == fault.at() {
        return fault;
    }
    let note = format!("{what}, {}, is made at", value.shown());
    fault.note(note, Some(value.span))
}

// What `str` makes of the argument: an int in decimal, or a string itself.
fn text(function: &str, argument: &(Span, Value)) -> Result<String, Fault> {
    match &argument.1.kind {
        Kind::Int(int) => Ok(format!("{int}")),
        Kind::Str(text) => Ok(String::from(&**text)),
        Kind::None
        | Kind::Bool(_)
        | Kind::List(_)
        | Kind::Tuple(_)
        | Kind::Dict(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => Err(narrowed(function, argument, "an int or a string")),
    }
}

fn int(argument: &(Span, Value)) -> Result<i64, Fault> {
    let (at, value) = argument;
    let text = match &value.kind {
        Kind::Int(int) => return Ok(*int),
        Kind::Bool(bool) => return Ok(i64::from(*bool)),
        Kind::Str(text) => text,
        Kind::None
        | Kind::List(_)
        | Kind::Tuple(_)
        | Kind::Dict(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => {
            return Err(rejected(
                "int",
                argument,
                "an int, a bool or a string of digits",
            ));
        }
    };
    let message = match text.parse::<i64>() {
        Ok(int) => return Ok(int),
        Err(error) => match error.kind() {
            IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => refusal(
                format_args!("`int` of {}, outside -2^63 to 2^63 - 1,", value.shown()),
                "give it a smaller int, since ints are 64-bit",
            ),
            _ => format!(
                "`int` can't read {}: give it decimal digits, after `-` or `+` or neither",
                value.shown()
            ),
        },
    };
    Err(origin(Fault::new(*at, message), "the argument", value))
}

fn sorted(argument: &(Span, Value), budget: &Budget) -> Result<Nested<Value>, Fault> {
    let (at, value) = argument;
    let elements: Vec<Value> = match &value.kind {
        Kind::List(items) | Kind::Tuple(items) => items.to_vec(),
        Kind::Dict(dict) => dict.iter().map(Entry::key_value).collect(),
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Str(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => {
            return Err(rejected("sorted", argument, "a list, a tuple or a dict"));
        }
    };
    let keyed: Option<Vec<(Key, Value)>> = (elements.into_iter())
        .map(|element| Some((Key::of(&element).ok()?, element)))
        .collect();
    let one_kind = keyed.filter(|keyed| {
        keyed.iter().all(|(key, _)| matches!(key, Key::Int(_)))
            || keyed.iter().all(|(key, _)| matches!(key, Key::Str(_)))
    });
    let Some(mut keyed) = one_kind else {
        let message = refusal(
            format_args!(
                "`sorted` of {} of mixed or other values than ints or strings",
                value.kind.describe()
            ),
            "give it only ints, or only strings",
        );
        return Err(origin(Fault::new(*at, message), "the argument", value));
    };
    // Ints alone, or strings alone, compare as their keys do; the sort is stable.
    keyed.sort_by(|(left, _), (right, _)| left.cmp(right));
    let elements = keyed.into_iter().map(|(_, element)| element).collect();
    Nested::new(elements, *at, budget)
}

type DictMethod = fn(&str, &Dict, &Call<'_>) -> Result<Value, Fault>;

type StringMethod = fn(&str, &str, &(Span, Value), &Call<'_>) -> Result<Kind, Fault>;

const DICT_METHODS: [(&str, DictMethod); 4] = [
    ("get", get),
    ("items", items),
    ("keys", keys),
    ("values", values),
];

const STRING_METHODS: [(&str, StringMethod); 6] = [
    ("endswith", ends_with),
    ("format", format),
    ("join", join),
    ("replace", replace),
    ("split", split),
    ("startswith", starts_with),
];

type ModuleMethod = fn(&Call<'_>) -> Result<Value, Fault>;

const JSON_METHODS: [(&str, ModuleMethod); 2] = [("decode", decode), ("encode", encode)];

// The functions of Bazel's and starlark-go's `json` module that the subset leaves out.
const JSON_LEFT_OUT: [&str; 2] = ["encode_indent", "indent"];

// Methods that would change a value, which no value allows.
const DICT_CHANGING: [&str; 5] = ["clear", "pop", "popitem", "setdefault", "update"];
const LIST_CHANGING: [&str; 6] = ["append", "clear", "extend", "insert", "pop", "remove"];

// Starlark's other methods of a list, and of a string, which the subset leaves out.
const LIST_LEFT_OUT: [&str; 1] = ["index"];
const STRING_LEFT_OUT: [&str; 26] = [
    "capitalize",
    "count",
    "elems",
    "find",
    "index",
    "isalnum",
    "isalpha",
    "isdigit",
    "islower",
    "isspace",
    "istitle",
    "isupper",
    "lower",
    "lstrip",
    "partition",
    "removeprefix",
    "removesuffix",
    "rfind",
    "rindex",
    "rpartition",
    "rsplit",
    "rstrip",
    "splitlines",
    "strip",
    "title",
    "upper",
];

// What calling the method `name` on the receiver gives. A method that the subset lacks is refused
// with the receiver's methods, or with a function to give the value to, where it has none.
pub(crate) fn method(
    receiver: &(Span, Value),
    name: &Name,
    call: &Call<'_>,
) -> Result<Value, Fault> {
    let method = name.text.as_str();
    let (methods, left_out): (Vec<&str>, &[&str]) = match &receiver.1.kind {
        Kind::Dict(dict) => {
            if let Some((_, run)) = DICT_METHODS.iter().find(|(has, _)| *has == method) {
                return run(method, dict, call);
            }
            if DICT_CHANGING.contains(&method) {
                return Err(changes(name, "dict", "make a new dict with `|`"));
            }
            (DICT_METHODS.iter().map(|(has, _)| *has).collect(), &[])
        }
        Kind::Str(text) => {
            if let Some((_, run)) = STRING_METHODS.iter().find(|(has, _)| *has == method) {
                let kind = run(method, text, receiver, call)?;
                return Ok(Value {
                    kind,
                    span: call.at,
                });
            }
            let methods = STRING_METHODS.iter().map(|(has, _)| *has).collect();
            (methods, &STRING_LEFT_OUT)
        }
        Kind::List(_) => {
            if LIST_CHANGING.contains(&method) {
                return Err(changes(name, "list", "make a new list with `+`"));
            }
            (Vec::new(), &LIST_LEFT_OUT)
        }
        Kind::Module(Module::Json) => {
            if let Some((_, run)) = JSON_METHODS.iter().find(|(has, _)| *has == method) {
                return run(call);
            }
            (
                JSON_METHODS.iter().map(|(has, _)| *has).collect(),
                &JSON_LEFT_OUT,
            )
        }
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Tuple(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_) => (Vec::new(), &[]),
    };
    let what = receiver.1.kind.describe();
    let instead = if methods.is_empty() {
        String::from("give it to a function, as `len`")
    } else {
        format!("call one of {}", list(&methods))
    };
    let message = if left_out.contains(&method) {
        refusal(format_args!("`{method}` of {what}"), instead)
    } else if methods.is_empty() {
        format!("{what} has no method `{method}`: {instead}")
    } else {
        format!("`{method}` is no method of {what}: {instead}")
    };
    Err(origin(
        Fault::new(name.span, message),
        "the receiver",
        &receiver.1,
    ))
}

fn changes(name: &Name, kind: &str, instead: &str) -> Fault {
    let message = refusal(
        format_args!("`{}`, which would change the {kind},", name.text),
        format_args!("{instead}, as values never change"),
    );
    Fault::new(name.span, message)
}

fn made_list(values: Vec<Value>, call: &Call<'_>) -> Result<Value, Fault> {
    let kind = Kind::List(Nested::new(values, call.at, call.budget)?);
    Ok(Value {
        kind,
        span: call.at,
    })
}

fn keys(name: &str, dict: &Dict, call: &Call<'_>) -> Result<Value, Fault> {
    positional::<0>(name, Takes::exactly(0), call)?;
    made_list(dict.iter().map(Entry::key_value).collect(), call)
}

fn values(name: &str, dict: &Dict, call: &Call<'_>) -> Result<Value, Fault> {
    positional::<0>(name, Takes::exactly(0), call)?;
    let values = dict.iter().map(|entry| entry.value.clone());
    made_list(values.collect(), call)
}

fn items(name: &str, dict: &Dict, call: &Call<'_>) -> Result<Value, Fault> {
    positional::<0>(name, Takes::exactly(0), call)?;
    let at = call.at;
    let pair = |entry: &Entry| {
        let pair = Vec::from([entry.key_value(), entry.value.clone()]);
        let kind = Kind::Tuple(Nested::new(pair, at, call.budget)?);
        Ok(Value { kind, span: at })
    };
    made_list(dict.iter().map(pair).collect::<Result<_, Fault>>()?, call)
}

fn get(name: &str, dict: &Dict, call: &Call<'_>) -> Result<Value, Fault> {
    let keywords: Vec<Keyword<'_>> = call.keywords().collect();
    let (key, default) = match (call.positional.as_slice(), keywords.as_slice()) {
        ([key], []) => (key, None),
        ([key, default], []) => (key, Some(&default.1)),
        ([_], [keyword]) if keyword.name == "default" => {
            let message = refusal(
                format_args!("`{name}`'s `default` by name"),
                "give it by position, after the key",
            );
            return Err(Fault::new(keyword.span, message));
        }
        (_, _) => {
            let message = format!(
                "`{name}` takes 1 or 2 arguments, by position: give it a key, and a default or \
                 none"
            );
            return Err(Fault::new(call.at, message));
        }
    };
    let lookup =
        Key::of(&key.1).map_err(|message| origin(Fault::new(key.0, message), "the key", &key.1))?;
    Ok(match (dict.get(&lookup), default) {
        (Some(entry), _) => entry.value.clone(),
        (None, Some(default)) => default.clone(),
        (None, None) => Value {
            kind: Kind::None,
            span: call.at,
        },
    })
}

fn encode(call: &Call<'_>) -> Result<Value, Fault> {
    let [argument] = positional::<1>("json.encode", Takes::exactly(1), call)?;
    let text = json::encode(argument, call.budget)?;
    Ok(Value {
        kind: Kind::Str(Text::new(&text, call.at, call.budget)?),
        span: call.at,
    })
}

fn decode(call: &Call<'_>) -> Result<Value, Fault> {
    let starlark = Takes {
        min: 1,
        max: 2,
        keywords: &["default"],
    };
    let name = "json.decode";
    let [argument] = positional::<1>(name, starlark, call)?;
    let text = text_of(name, argument)?;
    json::decode(text, argument, call)
}

fn format(_: &str, text: &str, receiver: &(Span, Value), call: &Call<'_>) -> Result<Kind, Fault> {
    let text = format_text(text, receiver, call)?;
    Ok(Kind::Str(Text::new(&text, call.at, call.budget)?))
}

fn join(name: &str, separator: &str, _: &(Span, Value), call: &Call<'_>) -> Result<Kind, Fault> {
    let [argument] = positional::<1>(name, Takes::exactly(1), call)?;
    let (at, value) = argument;
    let items = match &value.kind {
        Kind::List(items) | Kind::Tuple(items) => items,
        Kind::Dict(_) => return Err(narrowed(name, argument, "a list or a tuple of strings")),
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Str(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => return Err(rejected(name, argument, "a list or a tuple of strings")),
    };
    let mut parts = Vec::with_capacity(items.len());
    for item in items.iter() {
        let Kind::Str(text) = &item.kind else {
            let message = format!(
                "`{name}` can't join {}: give it a list or a tuple of strings",
                item.kind.describe()
            );
            return Err(origin(Fault::new(*at, message), "the element", item));
        };
        parts.push(&**text);
    }
    let separators = separator.len() * parts.len().saturating_sub(1);
    let bytes = parts.iter().map(|part| part.len()).sum::<usize>() + separators;
    call.budget.fits(bytes, call.at)?;
    let joined = parts.join(separator);
    Ok(Kind::Str(Text::new(&joined, call.at, call.budget)?))
}

fn split(name: &str, text: &str, _: &(Span, Value), call: &Call<'_>) -> Result<Kind, Fault> {
    let starlark = Takes {
        min: 0,
        max: 2,
        keywords: &["sep", "maxsplit"],
    };
    let [separator] = positional::<1>(name, starlark, call)?;
    // `None` splits at whitespace in Starlark.
    if let Kind::None = separator.1.kind {
        return Err(narrowed(name, separator, "a string"));
    }
    let separator_text = text_of(name, separator)?;
    if separator_text.is_empty() {
        let message = "`split` can't split at an empty separator: give it one";
        let fault = Fault::new(separator.0, message);
        return Err(origin(fault, "the separator", &separator.1));
    }
    let separators = text.matches(separator_text).count();
    let texts = text.len() - separators * separator_text.len();
    let elements = (separators + 1) * <Value as Element>::BYTES;
    call.budget.fits(texts + elements, call.at)?;
    let parts = text.split(separator_text).map(|part| {
        let kind = Kind::Str(Text::new(part, call.at, call.budget)?);
        Ok(Value {
            kind,
            span: call.at,
        })
    });
    let parts = parts.collect::<Result<_, Fault>>()?;
    Ok(Kind::List(Nested::new(parts, call.at, call.budget)?))
}

fn starts_with(name: &str, text: &str, _: &(Span, Value), call: &Call<'_>) -> Result<Kind, Fault> {
    let [prefix] = positional::<1>(name, AFFIX, call)?;
    Ok(Kind::Bool(text.starts_with(affix(name, prefix)?)))
}

fn ends_with(name: &str, text: &str, _: &(Span, Value), call: &Call<'_>) -> Result<Kind, Fault> {
    let [suffix] = positional::<1>(name, AFFIX, call)?;
    Ok(Kind::Bool(text.ends_with(affix(name, suffix)?)))
}

// Starlark's `startswith` and `endswith` take the affix, and then where to start and stop.
const AFFIX: Takes = Takes {
    min: 1,
    max: 3,
    keywords: &[],
};

fn affix<'v>(function: &str, argument: &'v (Span, Value)) -> Result<&'v str, Fault> {
    // Starlark tests each affix of a tuple.
    if let Kind::Tuple(_) = argument.1.kind {
        return Err(narrowed(function, argument, "a string"));
    }
    text_of(function, argument)
}

fn replace(name: &str, text: &str, _: &(Span, Value), call: &Call<'_>) -> Result<Kind, Fault> {
    let starlark = Takes {
        min: 2,
        max: 3,
        keywords: &[],
    };
    let [old, new] = positional::<2>(name, starlark, call)?;
    let old_text = text_of(name, old)?;
    if old_text.is_empty() {
        let message = refusal(
            "`replace` of an empty string, which Starlark's implementations insert between \
             different elements,",
            "give it the text to replace",
        );
        return Err(origin(Fault::new(old.0, message), "the string", &old.1));
    }
    let new_text = text_of(name, new)?;
    let replaced = text.matches(old_text).count();
    let bytes = text.len() - replaced * old_text.len() + replaced * new_text.len();
    call.budget.fits(bytes, call.at)?;
    let text = text.replace(old_text, new_text);
    Ok(Kind::Str(Text::new(&text, call.at, call.budget)?))
}

fn text_of<'v>(function: &str, argument: &'v (Span, Value)) -> Result<&'v str, Fault> {
    string(
        format_args!("`{function}`'s argument"),
        argument.0,
        &argument.1,
    )
}

pub(crate) fn string(what: impl fmt::Display, at: Span, value: &Value) -> Result<&str, Fault> {
    let Kind::Str(text) = &value.kind else {
        let message = format!("{what} can't be {}: give a string", value.shown());
        return Err(origin(Fault::new(at, message), "the argument", value));
    };
    Ok(text)
}

// The string `receiver.format(…)` makes (spec.md, "string·format"): each field replaced by the
// argument it names, as `str` makes it. The faults of the template are at `receiver`.
fn format_text(template: &str, receiver: &(Span, Value), call: &Call<'_>) -> Result<String, Fault> {
    let reject = |message: &str| unformatted(receiver, message);
    let mut text = String::new();
    let mut fields = Fields {
        receiver,
        call,
        numbered: None,
        next: 0,
    };
    let mut rest = template;
    while let Some(brace) = rest.find(['{', '}']) {
        text.push_str(&rest[..brace]);
        let (marks, after) = (&rest[brace..], &rest[brace + 1..]);
        if marks.starts_with("{{") || marks.starts_with("}}") {
            text.push_str(&marks[..1]);
            rest = &after[1..];
            continue;
        }
        if marks.starts_with('}') {
            let message = "this `format` string has a `}` that closes no field: write `}}` for a \
                           `}`";
            return Err(reject(message));
        }
        let Some(close) = after.find('}') else {
            let message = "this `format` string has a `{` that opens no field: close it with \
                           `}`, or write `{{` for a `{`";
            return Err(reject(message));
        };
        let argument = fields.argument(&after[..close])?;
        // The fields may repeat an argument, so the text may outgrow the template's.
        let field = self::text("format", &argument)?;
        call.budget.fits(text.len() + field.len(), call.at)?;
        text.push_str(&field);
        rest = &after[close + 1..];
    }
    text.push_str(rest);
    Ok(text)
}

fn unformatted(receiver: &(Span, Value), message: &str) -> Fault {
    let (at, template) = receiver;
    origin(Fault::new(*at, message), "the string", template)
}

struct Fields<'c, 'a> {
    receiver: &'c (Span, Value),
    call: &'c Call<'a>,
    /// Whether the fields are numbered, once one says: numbered and automatic ones don't mix.
    numbered: Option<bool>,
    /// The argument that the next automatic field takes.
    next: usize,
}

impl Fields<'_, '_> {
    fn argument(&mut self, field: &str) -> Result<(Span, Value), Fault> {
        let reject = |message: String| unformatted(self.receiver, &message);
        let shown = escape(field);
        if is_name(field) {
            let found = self.call.keywords().find(|keyword| keyword.name == field);
            let Some(keyword) = found else {
                return Err(reject(format!(
                    "`{{{shown}}}` names no keyword argument of this `format` call: give it \
                     `{shown} = …`"
                )));
            };
            return Ok((keyword.span, keyword.value.clone()));
        }
        if let Some(zeros) = field.strip_prefix('-')
            && !zeros.is_empty()
            && zeros.bytes().all(|byte| byte == b'0')
        {
            return Err(reject(refusal(
                format_args!(
                    "`{{{shown}}}`, a field that some Starlark implementations read as `{{0}}`,"
                ),
                "write `{0}`",
            )));
        }
        if !field.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(reject(format!(
                "`{{{shown}}}` is no field of a `format` string: write `{{}}`, `{{0}}` or \
                 `{{name}}`"
            )));
        }
        let numbered = !field.is_empty();
        if *self.numbered.get_or_insert(numbered) != numbered {
            return Err(reject(String::from(
                "this `format` string has numbered fields and `{}` fields: number every field, \
                 or none",
            )));
        }
        let index = if numbered {
            // Decimal, whatever zeros lead it.
            field.parse::<usize>().ok()
        } else {
            self.next += 1;
            Some(self.next - 1)
        };
        let given = self.call.positional.len();
        let argument = index.and_then(|index| self.call.positional.get(index));
        argument.cloned().ok_or_else(|| {
            reject(format!(
                "`{{{shown}}}` names no argument of this `format` call, which has {given} by \
                 position: give it one more"
            ))
        })
    }
}
