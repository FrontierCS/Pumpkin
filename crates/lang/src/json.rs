//! The `json` module's `encode` and `decode`, between the language's values and the text of RFC
//! 8259's JSON, as the module of Bazel's and starlark-go's Starlark has them
//! (<https://bazel.build/rules/lib/core/json>), but for what the subset refuses: an int past 64
//! bits, an object that gives a key twice, and a string with a lone surrogate, which a Rust
//! string can't hold. `value_of` makes the value of JSON that `serde_json` has read, with
//! `decode`'s refusals of an int and of nesting.
//!
//! All keep to the evaluation's bounds, and ask the byte budget for what they will hold before
//! they make it: `encode`'s text as it grows, `decode`'s values and the elements of the arrays
//! and objects it has open. `decode` and `value_of` refuse an array or an object nested deeper
//! than a value may be, `MAX_NESTING`, before they read further.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::MAX_NESTING;
use crate::builtins::{Call, origin};
use crate::problem::{Fault, refusal};
use crate::value::{
    Budget, Dict, Element, Entry, Key, Kind, Nested, Span, Text, Value, escape, float_text,
};

/// The compact JSON text of `argument`'s value, whose dicts' keys are in sorted order, so that
/// equal values give equal text. A value with no JSON form, NaN and the infinities among them,
/// and a dict with a key but a string, are refused.
// Keys sort by their bytes, as starlark-go's `json.encode` sorts them. Bazel's sorts by UTF-16
// code units, which orders a key's character from U+10000 before one from U+E000 to U+FFFF,
// where bytes order it after, so the two implementations differ there:
// https://github.com/google/starlark-go/blob/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/lib/json/json.go#L163-L186
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/lib/json/Json.java#L150-L159
pub(crate) fn encode(argument: &(Span, Value), budget: &Budget) -> Result<String, Fault> {
    let mut text = String::new();
    write(&mut text, &argument.1, argument.0, budget)?;
    Ok(text)
}

fn write(text: &mut String, value: &Value, at: Span, budget: &Budget) -> Result<(), Fault> {
    match &value.kind {
        Kind::None => push(text, "null", at, budget)?,
        Kind::Bool(true) => push(text, "true", at, budget)?,
        Kind::Bool(false) => push(text, "false", at, budget)?,
        Kind::Int(int) => push(text, &format!("{int}"), at, budget)?,
        // A float as starlark-rust's `json.encode` writes it, through `serde_json`, but that NaN
        // and the infinities are refused, as Bazel's and starlark-go's refuse them, not `null`:
        // https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/lib/json/Json.java#L141-L146
        Kind::Float(float) => {
            let Some(number) = serde_json::Number::from_f64(*float) else {
                let message = format!(
                    "`json.encode` can't encode {}, as JSON has no NaN or infinity: give it \
                     finite floats",
                    value.shown()
                );
                return Err(origin(Fault::new(at, message), "the value", value));
            };
            push(text, &format!("{number}"), at, budget)?;
        }
        Kind::Str(string) => quote(text, string, at, budget)?,
        Kind::List(items) | Kind::Tuple(items) => {
            push(text, "[", at, budget)?;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    push(text, ",", at, budget)?;
                }
                write(text, item, at, budget)?;
            }
            push(text, "]", at, budget)?;
        }
        Kind::Dict(dict) => {
            let mut entries = Vec::with_capacity(dict.len());
            for entry in dict.iter() {
                let Key::Str(key) = &entry.key else {
                    let message = format!(
                        "`json.encode` can't encode a dict keyed by `{}`: key each dict it \
                         encodes with strings",
                        entry.key.literal()
                    );
                    return Err(origin(
                        Fault::new(at, message),
                        "the key",
                        &entry.key_value(),
                    ));
                };
                entries.push((key, &entry.value));
            }
            // A dict's keys are unique, so the sort's stability is moot.
            entries.sort_unstable_by_key(|&(key, _)| key);
            push(text, "{", at, budget)?;
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    push(text, ",", at, budget)?;
                }
                quote(text, key, at, budget)?;
                push(text, ":", at, budget)?;
                write(text, value, at, budget)?;
            }
            push(text, "}", at, budget)?;
        }
        Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_)
        | Kind::Hosted(_) => {
            let message = format!(
                "`json.encode` can't encode {}: give it `None`, bools, numbers and strings, and \
                 lists, tuples and dicts of them",
                value.kind.describe()
            );
            return Err(origin(Fault::new(at, message), "the value", value));
        }
    }
    Ok(())
}

// Values that share their elements can write a text far longer than they take, and the budget
// counts the text only once it is a value, so each piece is asked of the budget before it is
// written.
fn push(text: &mut String, piece: &str, at: Span, budget: &Budget) -> Result<(), Fault> {
    budget.fits(text.len() + piece.len(), at)?;
    text.push_str(piece);
    Ok(())
}

// As Bazel's `appendQuoted` escapes, and starlark-go's `encode`, which in a string that is not
// all printable ASCII also escapes `<`, `>`, `&`, U+2028 and U+2029, as Go's `json.Marshal`
// does; but for U+007F, which Bazel writes raw and starlark-go as `\x7f`, which is no JSON.
// Escaping U+007F makes the text also a TOML basic string, which may hold no raw U+007F:
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/lib/json/Json.java#L233-L287
// https://github.com/google/starlark-go/blob/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/lib/json/json.go#L100-L111
fn quote(text: &mut String, string: &str, at: Span, budget: &Budget) -> Result<(), Fault> {
    // The bytes the escaped string takes, so that the budget refuses before anything is written.
    let escaped = |char: char| match char {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        '\0'..='\u{1f}' | '\u{7f}' => 6,
        char => char.len_utf8(),
    };
    let bytes = 2 + string.chars().map(escaped).sum::<usize>();
    budget.fits(text.len() + bytes, at)?;
    text.push('"');
    for char in string.chars() {
        match char {
            '"' => text.push_str("\\\""),
            '\\' => text.push_str("\\\\"),
            '\u{8}' => text.push_str("\\b"),
            '\u{c}' => text.push_str("\\f"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            '\0'..='\u{1f}' | '\u{7f}' => {
                // Writing to a `String` can't fail.
                let _ = write!(text, "\\u{:04x}", u32::from(char));
            }
            char => text.push(char),
        }
    }
    text.push('"');
    Ok(())
}

/// The value that the JSON `text` holds, each of whose parts has the span of the call. `text` is
/// the call's `argument`, which a message names. An int outside 64 bits and an object that
/// repeats a key are refused.
// After starlark-go's `decode`, which reads the text as this does, byte by byte:
// https://github.com/google/starlark-go/blob/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/lib/json/json.go#L316-L556
pub(crate) fn decode(
    text: &str,
    argument: &(Span, Value),
    call: &Call<'_>,
) -> Result<Value, Fault> {
    let mut decoder = Decoder {
        text,
        offset: 0,
        depth: 0,
        pending_bytes: 0,
        argument,
        call,
    };
    let value = decoder.value()?;
    if decoder.skip_space() {
        return Err(decoder.unexpected("the end of the text"));
    }
    Ok(value)
}

struct Decoder<'d> {
    text: &'d str,
    offset: usize,
    /// How many arrays and objects are open.
    depth: usize,
    /// The bytes of the elements read so far into the arrays and objects that are open. The
    /// budget counts them only when their array or object is made, so each element is checked
    /// against those of all the open ones together, and a text that would make too much is
    /// refused before it is read through.
    pending_bytes: usize,
    argument: &'d (Span, Value),
    call: &'d Call<'d>,
}

impl Decoder<'_> {
    fn skip_space(&mut self) -> bool {
        let rest = &self.text.as_bytes()[self.offset..];
        let text = rest
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        self.offset += text.unwrap_or(rest.len());
        text.is_some()
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_space();
        self.text[self.offset..].chars().next()
    }

    fn value(&mut self) -> Result<Value, Fault> {
        let kind = match self.peek() {
            Some('"') => Kind::Str(self.string()?),
            Some('[') => self.array()?,
            Some('{') => self.object()?,
            Some('-' | '0'..='9') => self.number()?,
            Some(_) => {
                let words = [
                    ("null", Kind::None),
                    ("true", Kind::Bool(true)),
                    ("false", Kind::Bool(false)),
                ];
                let rest = &self.text[self.offset..];
                let Some((word, kind)) = words.into_iter().find(|(word, _)| rest.starts_with(word))
                else {
                    return Err(self.unexpected("a value"));
                };
                self.offset += word.len();
                kind
            }
            None => return Err(self.unexpected("a value")),
        };
        Ok(self.made(kind))
    }

    fn made(&self, kind: Kind) -> Value {
        Value {
            kind,
            span: self.call.at,
        }
    }

    fn open(&mut self) -> Result<(), Fault> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            let message = too_deep(format_args!(
                "`json.decode`'s text nests arrays and objects {} deep at offset {}",
                self.depth, self.offset
            ));
            return Err(self.fault(message));
        }
        self.offset += 1;
        Ok(())
    }

    fn pend(&mut self, bytes: usize) -> Result<(), Fault> {
        self.call
            .budget
            .fits(self.pending_bytes + bytes, self.call.at)?;
        self.pending_bytes += bytes;
        Ok(())
    }

    fn close(&mut self) {
        self.depth -= 1;
        self.offset += 1;
    }

    fn more(&mut self, close: char, expected: &str) -> Result<bool, Fault> {
        match self.peek() {
            Some(',') => {
                self.offset += 1;
                Ok(true)
            }
            Some(char) if char == close => {
                self.close();
                Ok(false)
            }
            Some(_) | None => Err(self.unexpected(expected)),
        }
    }

    fn array(&mut self) -> Result<Kind, Fault> {
        self.open()?;
        let mut items = Vec::new();
        if self.peek() == Some(']') {
            self.close();
        } else {
            loop {
                self.pend(<Value as Element>::BYTES)?;
                items.push(self.value()?);
                if !self.more(']', "`,` or `]`")? {
                    break;
                }
            }
        }
        // `Nested::new` charges the items from here.
        self.pending_bytes -= items.len() * <Value as Element>::BYTES;
        Ok(Kind::List(Nested::new(
            items,
            self.call.at,
            self.call.budget,
        )?))
    }

    fn object(&mut self) -> Result<Kind, Fault> {
        self.open()?;
        let mut entries = Vec::new();
        let mut keys = BTreeSet::new();
        if self.peek() == Some('}') {
            self.close();
        } else {
            loop {
                if self.peek() != Some('"') {
                    return Err(self.unexpected("a key, a string"));
                }
                let key_offset = self.offset;
                let key = self.string()?;
                if !keys.insert(key.clone()) {
                    // Bazel's and starlark-go's `json.decode` keep the last value given; the
                    // subset refuses the object instead of choosing.
                    let message = refusal(
                        format_args!(
                            "`json.decode` of an object that gives its key `\"{}\"` a second \
                             time, at offset {key_offset},",
                            escape(&key)
                        ),
                        "give each key once",
                    );
                    return Err(self.fault(message));
                }
                if self.peek() != Some(':') {
                    return Err(self.unexpected("`:`"));
                }
                self.offset += 1;
                let value = self.value()?;
                self.pend(<Entry as Element>::BYTES)?;
                entries.push(Entry {
                    key: Key::Str(key),
                    key_span: self.call.at,
                    value,
                });
                if !self.more('}', "`,` or `}`")? {
                    break;
                }
            }
        }
        // `Dict::new` charges the entries from here.
        self.pending_bytes -= entries.len() * <Entry as Element>::BYTES;
        Ok(Kind::Dict(Dict::new(
            entries,
            self.call.at,
            self.call.budget,
        )?))
    }

    fn string(&mut self) -> Result<Text, Fault> {
        self.offset += 1;
        let mut string = String::new();
        loop {
            let rest = &self.text[self.offset..];
            let Some(char) = rest.chars().next() else {
                return Err(self.unexpected("`\"`, which closes the string"));
            };
            match char {
                '"' => {
                    self.offset += 1;
                    return Text::new(&string, self.call.at, self.call.budget);
                }
                '\\' => string.push(self.escaped()?),
                '\0'..='\u{1f}' => {
                    return Err(self.unexpected("a character of the string, or an escape for it"));
                }
                char => {
                    string.push(char);
                    self.offset += char.len_utf8();
                }
            }
        }
    }

    fn escaped(&mut self) -> Result<char, Fault> {
        let start = self.offset;
        let escape = self.text[start + 1..].chars().next();
        let char = match escape {
            Some(char @ ('"' | '\\' | '/')) => char,
            Some('b') => '\u{8}',
            Some('f') => '\u{c}',
            Some('n') => '\n',
            Some('r') => '\r',
            Some('t') => '\t',
            Some('u') => return self.unicode(),
            Some(_) | None => {
                self.offset += 1;
                return Err(
                    self.unexpected("an escape: `\"`, `\\`, `/`, `b`, `f`, `n`, `r`, `t` or `u`")
                );
            }
        };
        self.offset += 2;
        Ok(char)
    }

    fn unicode(&mut self) -> Result<char, Fault> {
        let start = self.offset;
        let first = self.code_unit()?;
        let code = match first {
            0xd800..=0xdbff => {
                let low = self.text[self.offset..]
                    .starts_with("\\u")
                    .then(|| self.code_unit());
                match low.transpose()? {
                    Some(low @ 0xdc00..=0xdfff) => {
                        0x10000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
                    }
                    Some(_) | None => return Err(self.lone(start)),
                }
            }
            0xdc00..=0xdfff => return Err(self.lone(start)),
            unit => u32::from(unit),
        };
        let Some(char) = char::from_u32(code) else {
            unreachable!("{code:#x} is no character, though it is no surrogate and below 0x110000")
        };
        Ok(char)
    }

    fn code_unit(&mut self) -> Result<u16, Fault> {
        self.offset += 2;
        let mut unit = 0;
        for _ in 0..4 {
            let digit = match self.text.as_bytes().get(self.offset).copied() {
                Some(byte @ b'0'..=b'9') => byte - b'0',
                Some(byte @ b'a'..=b'f') => byte - b'a' + 10,
                Some(byte @ b'A'..=b'F') => byte - b'A' + 10,
                _ => return Err(self.unexpected("a hexadecimal digit, as `\\u` takes four")),
            };
            unit = (unit << 4) | u16::from(digit);
            self.offset += 1;
        }
        Ok(unit)
    }

    fn lone(&self, start: usize) -> Fault {
        let message = refusal(
            format_args!(
                "`json.decode` of a string that holds a UTF-16 surrogate of no pair, escaped at \
                 offset {start},"
            ),
            "escape a character outside the Basic Multilingual Plane as a surrogate pair",
        );
        self.fault(message)
    }

    // A float where the text is no int, as starlark-rust's `json.decode` reads one:
    // https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/stdlib/json.rs#L252-L265
    fn number(&mut self) -> Result<Kind, Fault> {
        let start = self.offset;
        let rest = &self.text[start..];
        // As starlark-go's `decode` scans a number before it checks it, so that a message shows
        // all of it.
        let number = rest
            .find(|char: char| !matches!(char, '0'..='9' | '-' | '+' | '.' | 'e' | 'E'))
            .map_or(rest, |end| &rest[..end]);
        let digits = number.strip_prefix('-').unwrap_or(number);
        let integer = digits.starts_with(|char: char| char.is_ascii_digit())
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && !(digits.starts_with('0') && digits.len() > 1);
        if integer && let Ok(int) = number.parse::<i64>() {
            self.offset += number.len();
            return Ok(Kind::Int(int));
        }
        if !integer && let Ok(float) = serde_json::from_str::<f64>(number) {
            self.offset += number.len();
            return Ok(Kind::Float(float));
        }
        let shown = escape(number);
        let message = if integer {
            past_64_bits(format_args!(
                "`json.decode` of `{shown}`, at offset {start}"
            ))
        } else {
            format!(
                "`json.decode` reads `{shown}` at offset {start} of its text, which is no JSON \
                 number a float holds: write a number as JSON does, of at most {}",
                float_text(f64::MAX)
            )
        };
        Err(self.fault(message))
    }

    fn unexpected(&self, expected: &str) -> Fault {
        let found = match self.text[self.offset..].chars().next() {
            Some(char) => {
                let mut shown = String::new();
                shown.push(char);
                format!("`{}`", escape(&shown))
            }
            None => String::from("the text's end"),
        };
        let message = format!(
            "`json.decode` reads {found} at offset {} of its text, where JSON has {expected}: \
             give it JSON text",
            self.offset
        );
        self.fault(message)
    }

    fn fault(&self, message: String) -> Fault {
        let (at, text) = self.argument;
        origin(Fault::new(*at, message), "the text", text)
    }
}

/// The value that `json`, which `serde_json` has read, holds, each part made at `at`: a number
/// that `serde_json` holds as an int is an int, and any other a float. An int outside 64 bits is
/// refused, as `decode` refuses it.
pub(crate) fn value_of(
    json: &serde_json::Value,
    at: Span,
    budget: &Budget,
) -> Result<Value, Fault> {
    nested_value_of(json, at, budget, 0)
}

// `value_of`, of a part `depth` deep in the value it is part of.
fn nested_value_of(
    json: &serde_json::Value,
    at: Span,
    budget: &Budget,
    depth: usize,
) -> Result<Value, Fault> {
    let kind = match json {
        serde_json::Value::Null => Kind::None,
        serde_json::Value::Bool(bool) => Kind::Bool(*bool),
        serde_json::Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => Kind::Int(int),
            (None, Some(unsigned)) => {
                let message = past_64_bits(format_args!("`{unsigned}`"));
                return Err(Fault::new(at, message));
            }
            (None, None) => {
                let Some(float) = number.as_f64() else {
                    unreachable!(
                        "`serde_json` holds a number that is no 64-bit int as a float, so it gives \
                         that float, as the workspace leaves its `arbitrary_precision` feature off"
                    )
                };
                Kind::Float(float)
            }
        },
        serde_json::Value::String(text) => Kind::Str(Text::new(text, at, budget)?),
        // So that the walk's stack is bounded as a value's nesting is.
        serde_json::Value::Array(_) | serde_json::Value::Object(_) if depth == MAX_NESTING => {
            let message = too_deep(format_args!(
                "this JSON nests arrays and objects more than {MAX_NESTING} deep"
            ));
            return Err(Fault::new(at, message));
        }
        serde_json::Value::Array(items) => {
            let items = (items.iter())
                .map(|item| nested_value_of(item, at, budget, depth + 1))
                .collect::<Result<_, _>>()?;
            Kind::List(Nested::new(items, at, budget)?)
        }
        serde_json::Value::Object(entries) => {
            let mut made = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                made.push(Entry {
                    key: Key::Str(Text::new(key, at, budget)?),
                    key_span: at,
                    value: nested_value_of(value, at, budget, depth + 1)?,
                });
            }
            Kind::Dict(Dict::new(made, at, budget)?)
        }
    };
    Ok(Value { kind, span: at })
}

// The refusal of `number`, an int outside 64 bits.
fn past_64_bits(number: fmt::Arguments<'_>) -> String {
    refusal(
        format_args!("{number}, outside -2^63 to 2^63 - 1,"),
        "give smaller ints, since ints are 64-bit",
    )
}

// The refusal of `nesting`, arrays and objects nested deeper than a value may be.
fn too_deep(nesting: fmt::Arguments<'_>) -> String {
    format!("{nesting}: nest them at most {MAX_NESTING} deep")
}
