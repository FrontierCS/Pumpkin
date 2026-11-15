//! What `json.encode` and `json.decode` make: the module of Bazel's and starlark-go's Starlark
//! (<https://bazel.build/rules/lib/core/json>), which converts a value to the text of RFC 8259's
//! JSON and back, but for what the subset refuses: a float, which it lacks, an int past 64 bits,
//! an object that gives a key twice, and a string with a lone surrogate, which a Rust string
//! can't hold.
//!
//! Both are pure, and keep to the evaluation's bounds: what each will hold, `encode`'s text as it
//! grows and `decode`'s values and the elements of the arrays and objects it has open, is asked
//! of the byte budget before it is made, and `decode` refuses an array or an object nested deeper
//! than a value may be, `MAX_NESTING`, before it reads further.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::MAX_NESTING;
use crate::builtins::{Call, origin};
use crate::problem::{Fault, refusal};
use crate::value::{Budget, Dict, Element, Entry, Key, Kind, Nested, Span, Text, Value, escape};

/// The compact JSON text of `argument`, its value and the span of the expression that gives it:
/// `None` as `null`, a bool as `true` or `false`, an int in decimal, a string as [`quote`]
/// writes it, a list or a tuple as an array, and a dict as an object, its keys sorted by their
/// bytes.
///
/// # Errors
///
/// A dict keyed by an int, a function, an image or a module in the value; or a text that would
/// pass what `budget` has left.
// Sorted as starlark-go's `json.encode` sorts them. Bazel's sorts by UTF-16 code units, which
// orders a key's character from U+10000 before one from U+E000 to U+FFFF, where bytes order it
// after; the two implementations differ there, and this follows starlark-go:
// https://github.com/google/starlark-go/blob/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/lib/json/json.go#L163-L186
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/lib/json/Json.java#L150-L159
pub(crate) fn encode(argument: &(Span, Value), budget: &Budget) -> Result<String, Fault> {
    let mut text = String::new();
    write(&mut text, &argument.1, argument.0, budget)?;
    Ok(text)
}

/// Writes `value`, of the argument at `at`, at the end of `text`.
fn write(text: &mut String, value: &Value, at: Span, budget: &Budget) -> Result<(), Fault> {
    match &value.kind {
        Kind::None => push(text, "null", at, budget)?,
        Kind::Bool(true) => push(text, "true", at, budget)?,
        Kind::Bool(false) => push(text, "false", at, budget)?,
        Kind::Int(int) => push(text, &format!("{int}"), at, budget)?,
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
        Kind::Function(_) | Kind::Builtin(_) | Kind::Image(_) | Kind::Module(_) => {
            let message = format!(
                "`json.encode` can't encode {}: give it `None`, bools, ints and strings, and \
                 lists, tuples and dicts of them",
                value.kind.describe()
            );
            return Err(origin(Fault::new(at, message), "the value", value));
        }
    }
    Ok(())
}

/// Writes `piece` at the end of `text`, refused if the text would pass what `budget` has left.
// Values that share their elements can write a text far longer than they take, so each piece is
// asked of the budget before it is written.
fn push(text: &mut String, piece: &str, at: Span, budget: &Budget) -> Result<(), Fault> {
    budget.fits(text.len() + piece.len(), at)?;
    text.push_str(piece);
    Ok(())
}

/// Writes `string` at the end of `text` as a JSON string, refused if the text would pass what
/// `budget` has left: `"` and `\` escaped, `\b`, `\f`, `\n`, `\r` and `\t` for those
/// characters, `\u00XX` for every other one below U+0020 and for U+007F, and every other
/// character as it is. So the text is also a TOML basic string, which may hold no raw U+007F.
// As Bazel's `appendQuoted` escapes, and starlark-go's `encode`, which in a string that is not
// all printable ASCII also escapes `<`, `>`, `&`, U+2028 and U+2029, as Go's `json.Marshal` does;
// but for U+007F, which Bazel writes raw and starlark-go as `\x7f`, which is no JSON. Both write
// U+0080 to U+009F raw, as this does:
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/lib/json/Json.java#L233-L287
// https://github.com/google/starlark-go/blob/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/lib/json/json.go#L100-L111
fn quote(text: &mut String, string: &str, at: Span, budget: &Budget) -> Result<(), Fault> {
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

/// The value that `text`, the string `argument` gives `call`, denotes: `null` as `None`, `true`
/// and `false` as bools, an integer as an int, a string as a string, an array as a list, and an
/// object as a dict, its keys in the order the text gives them; each value made at the call.
///
/// # Errors
///
/// A text that is no JSON, or that the module doc says the subset refuses, at the first byte
/// that shows it; an array or an object nested deeper than `MAX_NESTING`; or values that pass
/// the budget.
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
    /// The byte of `text` to read next.
    offset: usize,
    /// How many of the arrays and objects that hold the value being read are open.
    depth: usize,
    /// The bytes of the elements read into the arrays and objects open, which the budget counts
    /// only once each closes.
    pending_bytes: usize,
    argument: &'d (Span, Value),
    call: &'d Call<'d>,
}

impl Decoder<'_> {
    /// Skips JSON's white space, and tells whether any text is left.
    fn skip_space(&mut self) -> bool {
        let rest = &self.text.as_bytes()[self.offset..];
        let text = rest
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        self.offset += text.unwrap_or(rest.len());
        text.is_some()
    }

    /// The character after white space, which is not read, if any is left.
    fn peek(&mut self) -> Option<char> {
        self.skip_space();
        self.text[self.offset..].chars().next()
    }

    fn value(&mut self) -> Result<Value, Fault> {
        let kind = match self.peek() {
            Some('"') => Kind::Str(self.string()?),
            Some('[') => self.array()?,
            Some('{') => self.object()?,
            Some('-' | '0'..='9') => Kind::Int(self.int()?),
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

    /// Reads the `[` or `{` that is next, refused if it would nest past `MAX_NESTING`.
    fn open(&mut self) -> Result<(), Fault> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            let message = format!(
                "`json.decode`'s text nests arrays and objects {} deep at offset {}: nest them at \
                 most {MAX_NESTING} deep",
                self.depth, self.offset
            );
            return Err(self.fault(message));
        }
        self.offset += 1;
        Ok(())
    }

    /// Counts `bytes` more of an element to be read into an array or an object open, refused if
    /// they and those of every element pending would pass what the budget has left.
    // So that all the arrays and objects open together, and not each alone, keep to the budget.
    fn pend(&mut self, bytes: usize) -> Result<(), Fault> {
        self.call
            .budget
            .fits(self.pending_bytes + bytes, self.call.at)?;
        self.pending_bytes += bytes;
        Ok(())
    }

    /// Reads the `]` or `}` that closes what `open` read last.
    fn close(&mut self) {
        self.depth -= 1;
        self.offset += 1;
    }

    /// Reads `,` and tells whether more follows, or reads `close` and tells that none does.
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
                    // Bazel's and starlark-go's `json.decode` keep the last value given.
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
        self.pending_bytes -= entries.len() * <Entry as Element>::BYTES;
        Ok(Kind::Dict(Dict::new(
            entries,
            self.call.at,
            self.call.budget,
        )?))
    }

    /// Reads the string whose `"` is next.
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

    /// Reads the escape whose `\` is next, and gives the character it denotes.
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

    /// Reads the `\uXXXX` escape that is next, and the one after it if they make a surrogate pair,
    /// and gives the character they denote.
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

    /// Reads the `\uXXXX` escape that is next, and gives its UTF-16 code unit.
    fn code_unit(&mut self) -> Result<u16, Fault> {
        let start = self.offset + 2;
        let rest = &self.text.as_bytes()[start..];
        let digits = rest
            .iter()
            .take(4)
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
        self.offset = start + digits;
        if digits < 4 {
            return Err(self.unexpected("a hexadecimal digit, as `\\u` takes four"));
        }
        let Ok(unit) = u16::from_str_radix(&self.text[start..self.offset], 16) else {
            unreachable!("four hexadecimal digits make a 16-bit unit, at most 0xffff")
        };
        Ok(unit)
    }

    /// The refusal of the lone surrogate whose escape starts at `start`.
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

    /// Reads the number that is next, which the subset reads only as an int: `-` or none, then
    /// `0`, or a digit from `1` and any more.
    fn int(&mut self) -> Result<i64, Fault> {
        let start = self.offset;
        let rest = &self.text[start..];
        // As starlark-go's `decode` scans a number before it checks it.
        let number = rest
            .find(|char: char| !matches!(char, '0'..='9' | '-' | '+' | '.' | 'e' | 'E'))
            .map_or(rest, |end| &rest[..end]);
        let digits = number.strip_prefix('-').unwrap_or(number);
        let integer = digits.starts_with(|char: char| char.is_ascii_digit())
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && !(digits.starts_with('0') && digits.len() > 1);
        if integer && let Ok(int) = number.parse::<i64>() {
            self.offset += number.len();
            return Ok(int);
        }
        let shown = escape(number);
        let message = if integer {
            refusal(
                format_args!(
                    "`json.decode` of `{shown}`, at offset {start}, outside -2^63 to 2^63 - 1,"
                ),
                "give smaller ints, since ints are 64-bit",
            )
        } else if serde_json::from_str::<serde_json::Number>(number).is_ok() {
            refusal(
                format_args!("`json.decode` of `{shown}`, at offset {start}, a float,"),
                "give ints",
            )
        } else {
            format!(
                "`json.decode` reads `{shown}` at offset {start} of its text, which is no JSON \
                 number: write an int as decimal digits, after `-` or none, with no leading `0`"
            )
        };
        Err(self.fault(message))
    }

    /// The error of the character next, or of the text's end, where `expected` is.
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

    /// The fault, saying `message`, of the text that the call's argument gives.
    fn fault(&self, message: String) -> Fault {
        let (at, text) = self.argument;
        origin(Fault::new(*at, message), "the text", text)
    }
}
