//! A `workflow()` call's values read into the IR as an `ir::Workflow`, through a
//! `serde::Deserializer` over values, and a `Workflow::parse` error's pointer walked through the
//! same values.
//!
//! The kinds map one to one: a dict to a struct, or to an enum as its one key, the kind, and
//! that key's value (`{"judge": {…}}`); a list or a tuple to a sequence; a string to a string,
//! which the IR's types check; an int to an integer, which they range-check; a float to a float;
//! a bool to a bool; `None` to an absent option; an image to the string of its ID; and a function
//! that a `def` makes to a body, the dict of its file's label and its name. Any other function is
//! no data.
//! Messages say what `serde` names in the language's words: an int type, for one, by the ints it
//! holds.
//!
//! An error is at the value read when it happened, even where the value's own type refuses it
//! once read, as a name's does; for an unknown field or kind, it is at the key, and for a
//! missing field, at the dict. Its note is the place in the IR, as a JSON Pointer.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use schema::ir::{self, JsonPointer};
use schema::workflow::ParseError;
use serde::de::value::StrDeserializer;
use serde::de::{
    self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, Unexpected, VariantAccess, Visitor,
};
use serde::{Deserialize, forward_to_deserialize_any};

use crate::problem::{Fault, Sources, list};
use crate::syntax::Type;
use crate::value::{Entry, Function, Key, Kind, Span, Value, escape, quote};

/// The IR's workflow that `fields`, the dict of the keyword arguments of `workflow()`, gives. An
/// image in it is the ID at its place in `images`. A fault is at the value that is wrong, and
/// notes where in the IR it is.
pub(crate) fn workflow(
    fields: &Value,
    images: &[ir::Image],
    sources: &Sources,
) -> Result<ir::Workflow, Fault> {
    // The fields of `ir::Workflow` that a workflow's writer gives.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        name: ir::WorkflowName,
        roles: Vec<ir::Role>,
        score: ir::Score,
    }
    let de = De {
        value: fields,
        images,
        sources,
    };
    let Fields { name, roles, score } = Fields::deserialize(de).map_err(|error| {
        let Error {
            message,
            at,
            mut tokens,
        } = error;
        tokens.reverse();
        let pointer = (tokens.iter()).fold(JsonPointer::root(), |pointer, token| match token {
            Token::Key(key) => pointer.token(key),
            Token::Index(index) => pointer.token(index),
        });
        let message = message.read(&tokens);
        Fault::new(at.unwrap_or(fields.span), message).note(place(&pointer), None)
    })?;
    Ok(ir::Workflow {
        ir_version: ir::IrVersion::V0,
        name,
        roles,
        score,
    })
}

/// The faults of `errors`, which `schema` gives for the IR that `fields` made, each at the span of
/// the value its pointer reaches, or of the deepest one that exists, when the value lacks what
/// the pointer names. Errors with one message at one span make one fault, which notes each of
/// their pointers.
pub(crate) fn locate(fields: &Value, errors: &[ParseError]) -> Vec<Fault> {
    let mut located: Vec<(Span, &str, Vec<String>)> = Vec::new();
    for ParseError { at, message } in errors {
        let mut tokens = at.tokens();
        let mut reached = fields;
        while let Some(child) = tokens.next().and_then(|token| child(reached, &token)) {
            reached = child;
        }
        let same = |(span, said, _): &&mut (Span, &str, Vec<String>)| {
            *span == reached.span && *said == message.as_str()
        };
        match located.iter_mut().find(same) {
            Some((_, _, places)) => places.push(place(at)),
            None => located.push((reached.span, message, Vec::from([place(at)]))),
        }
    }
    (located.into_iter())
        .map(|(span, message, places)| {
            let fault = Fault::new(span, message);
            (places.into_iter()).fold(fault, |fault, place| fault.note(place, None))
        })
        .collect()
}

fn child<'v>(value: &'v Value, token: &str) -> Option<&'v Value> {
    match &value.kind {
        Kind::Dict(entries) => (entries.iter())
            .find(|entry| matches!(&entry.key, Key::Str(key) if **key == *token))
            .map(|entry| &entry.value),
        Kind::List(items) | Kind::Tuple(items) => items.get(token.parse::<usize>().ok()?),
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Float(_)
        | Kind::Str(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_)
        | Kind::Hosted(_) => None,
    }
}

fn place(pointer: &JsonPointer) -> String {
    if pointer.as_str().is_empty() {
        String::from("the whole IR")
    } else {
        format!("the IR's {pointer}")
    }
}

#[derive(Debug)]
struct Error {
    message: Message,
    /// The span of the innermost value or key at fault, if the error has met one.
    at: Option<Span>,
    /// The path to the value at fault, innermost first, as the error is passed up: the reverse
    /// of the order `Message::read` takes.
    tokens: Vec<Token>,
}

#[derive(Debug)]
enum Token {
    Key(String),
    Index(usize),
}

#[derive(Debug)]
enum Message {
    Whole(String),
    Invalid {
        found: String,
        expected: String,
    },
    Length {
        len: usize,
        expected: String,
    },
    UnknownField {
        field: String,
        expected: &'static [&'static str],
    },
    UnknownKind {
        kind: String,
        expected: &'static [&'static str],
    },
    Kinds {
        len: usize,
        expected: &'static [&'static str],
    },
    MissingField(&'static str),
}

impl Message {
    /// The message, naming the value at fault by `tokens`, its path from the top.
    fn read(&self, tokens: &[Token]) -> String {
        let place = match tokens {
            [] => String::from("the workflow"),
            [.., Token::Key(key), Token::Index(_)] => format!("an element of `{}`", escape(key)),
            [.., Token::Key(key)] => format!("`{}`", escape(key)),
            [.., Token::Index(_)] => String::from("an element"),
        };
        match self {
            Self::Whole(message) => message.clone(),
            Self::Invalid { found, expected } => {
                format!("{place} can't be {found}: expected {expected}")
            }
            Self::Length { len, expected } => {
                format!("{place} holds {len} elements: expected {expected}")
            }
            Self::UnknownField { field, expected } => format!(
                "`{}` is no field of {place}: expected one of {}",
                escape(field),
                list(expected)
            ),
            Self::UnknownKind { kind, expected } => format!(
                "`{}` is no kind of {place}: expected one of {}",
                escape(kind),
                list(expected)
            ),
            Self::Kinds { len, expected } => format!(
                "{place} holds {len} keys: give it one, its kind, one of {}",
                list(expected)
            ),
            Self::MissingField(field) => format!("{place} lacks `{field}`: give it"),
        }
    }
}

impl Error {
    fn new(message: Message) -> Self {
        Self {
            message,
            at: None,
            tokens: Vec::new(),
        }
    }

    /// Notes that the error is in the value at `token` of the one it was passed up from.
    fn at(mut self, at: Span, token: Token) -> Self {
        self.at.get_or_insert(at);
        self.tokens.push(token);
        self
    }

    /// Notes that the error is in a key, which has no token of its own.
    fn at_key(mut self, at: Span) -> Self {
        self.at.get_or_insert(at);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message.read(&self.tokens))
    }
}

impl core::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self::new(Message::Whole(message.to_string()))
    }

    fn invalid_type(unexpected: Unexpected<'_>, expected: &dyn de::Expected) -> Self {
        Self::invalid_value(unexpected, expected)
    }

    fn invalid_value(unexpected: Unexpected<'_>, expected: &dyn de::Expected) -> Self {
        let found = match unexpected {
            Unexpected::Bool(true) => String::from("`True`"),
            Unexpected::Bool(false) => String::from("`False`"),
            Unexpected::Unsigned(int) => format!("`{int}`"),
            Unexpected::Signed(int) => format!("`{int}`"),
            Unexpected::Str(text) => format!("`{}`", quote(text)),
            Unexpected::Unit => String::from("`None`"),
            Unexpected::Seq => String::from("a list"),
            Unexpected::Map => String::from("a dict"),
            unexpected @ (Unexpected::Float(_)
            | Unexpected::Char(_)
            | Unexpected::Bytes(_)
            | Unexpected::Option
            | Unexpected::NewtypeStruct
            | Unexpected::Enum
            | Unexpected::UnitVariant
            | Unexpected::NewtypeVariant
            | Unexpected::TupleVariant
            | Unexpected::StructVariant
            | Unexpected::Other(_)) => unexpected.to_string(),
        };
        let expected = words(expected);
        Self::new(Message::Invalid { found, expected })
    }

    fn invalid_length(len: usize, expected: &dyn de::Expected) -> Self {
        let expected = expected.to_string();
        Self::new(Message::Length { len, expected })
    }

    fn unknown_variant(kind: &str, expected: &'static [&'static str]) -> Self {
        let kind = kind.to_owned();
        Self::new(Message::UnknownKind { kind, expected })
    }

    fn unknown_field(field: &str, expected: &'static [&'static str]) -> Self {
        let field = field.to_owned();
        Self::new(Message::UnknownField { field, expected })
    }

    fn missing_field(field: &'static str) -> Self {
        Self::new(Message::MissingField(field))
    }
}

// `serde` names the IR's int types as Rust does, as `u32` or `a nonzero u64`, which the language
// has none of: they become the ints that the type holds.
fn words(expected: &dyn de::Expected) -> String {
    let expected = expected.to_string();
    let (least, rust) = match expected.strip_prefix("a nonzero ") {
        Some(rust) => (1, rust),
        None => (0, expected.as_str()),
    };
    let most = match rust {
        "u32" => u32::MAX,
        // Every int that isn't negative fits, since ints are 64-bit.
        "u64" if least == 1 => return String::from("a positive int"),
        "u64" => return String::from("an int of 0 or more"),
        _ => return expected,
    };
    format!("an int from {least} to {most}")
}

/// A [`Value`] that `serde` reads as the IR's JSON would be read: an image is its ID, a string,
/// and a function that a `def` makes is its body, `{"file": …, "def": …}`, its file's label in
/// `sources`.
#[derive(Clone, Copy)]
struct De<'v> {
    value: &'v Value,
    images: &'v [ir::Image],
    sources: &'v Sources,
}

impl<'v> De<'v> {
    fn of(self, value: &'v Value) -> Self {
        Self { value, ..self }
    }

    // The body that `function` is, `visitor`'s map: its file's label, its name, and the
    // arguments its caller gives, each positional parameter after the handle and the payload that
    // is annotated `str`, `int` or `bool` (`crate::check` refuses any other).
    fn body<'de, V: Visitor<'de>>(
        &self,
        function: &Function,
        visitor: V,
    ) -> Result<V::Value, Error> {
        let def = &*function.def;
        let mut body = serde_json::Map::new();
        let file = self.sources.label(function.module).to_string();
        body.insert(String::from("file"), serde_json::Value::String(file));
        let name = def.name.text.clone();
        body.insert(String::from("def"), serde_json::Value::String(name));
        let caller = (def.params.iter().zip(&function.defaults))
            .take(def.positional)
            .skip(2);
        let mut arguments = BTreeMap::new();
        for (param, default) in caller {
            let Some(argument) = argument(param.annotation.as_ref(), default.as_ref()) else {
                continue;
            };
            arguments.insert(param.name.text.clone(), argument);
        }
        if !arguments.is_empty() {
            let arguments = serde_json::to_value(arguments).map_err(de::Error::custom)?;
            body.insert(String::from("arguments"), arguments);
        }
        let body = serde_json::Value::Object(body);
        de::Deserializer::deserialize_map(body, visitor).map_err(de::Error::custom)
    }

    fn mismatch(&self, expected: &str) -> Error {
        let found = String::from(self.value.kind.describe());
        let expected = String::from(expected);
        Error::new(Message::Invalid { found, expected })
    }
}

macro_rules! forward {
    ($to:ident: $($method:ident)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
                self.$to(visitor)
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for De<'_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.value.kind {
            Kind::None => visitor.visit_unit(),
            Kind::Bool(bool) => visitor.visit_bool(*bool),
            Kind::Int(int) => visitor.visit_i64(*int),
            Kind::Float(float) => visitor.visit_f64(*float),
            Kind::Str(text) => visitor.visit_str(text),
            Kind::Image(image) => visitor.visit_str(self.images[image.0].as_str()),
            Kind::List(items) | Kind::Tuple(items) => seq(self, items, visitor),
            Kind::Dict(entries) => visitor.visit_map(Map::new(self, entries)),
            Kind::Function(function) => self.body(function, visitor),
            Kind::Builtin(_) | Kind::Module(_) | Kind::Hosted(_) => Err(self.mismatch("data")),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value.kind {
            Kind::Bool(bool) => visitor.visit_bool(bool),
            _ => Err(self.mismatch("a bool")),
        }
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value.kind {
            // The visitor checks the int against its type's range.
            Kind::Int(int) => match u64::try_from(int) {
                Ok(unsigned) => visitor.visit_u64(unsigned),
                Err(_) => visitor.visit_i64(int),
            },
            _ => Err(self.mismatch("an int")),
        }
    }

    forward! {
        deserialize_i64: deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i128
            deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64 deserialize_u128
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.value.kind {
            Kind::Str(text) => visitor.visit_str(text),
            Kind::Image(image) => visitor.visit_str(self.images[image.0].as_str()),
            _ => Err(self.mismatch("a string")),
        }
    }

    forward! {
        deserialize_str: deserialize_string deserialize_identifier
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value.kind {
            Kind::None => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value.kind {
            Kind::None => visitor.visit_unit(),
            _ => Err(self.mismatch("`None`")),
        }
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.value.kind {
            Kind::List(items) | Kind::Tuple(items) => seq(self, items, visitor),
            _ => Err(self.mismatch("a list")),
        }
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.value.kind {
            Kind::Dict(entries) => visitor.visit_map(Map::new(self, entries)),
            Kind::Function(function) => self.body(function, visitor),
            _ => Err(self.mismatch("a dict")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match &self.value.kind {
            // A string names a kind that holds no value.
            Kind::Str(text) => visitor.visit_enum(Named(text)),
            // A dict of one key names a kind by it, and gives the kind's value.
            Kind::Dict(entries) => match &**entries {
                [entry] => visitor.visit_enum(Variant {
                    de: self,
                    kind: name(entry, "kind")?,
                    entry,
                }),
                entries => Err(Error::new(Message::Kinds {
                    len: entries.len(),
                    expected: variants,
                })),
            },
            _ => Err(self.mismatch(&format!("a dict of one key, one of {}", list(variants)))),
        }
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        f32 f64 char bytes byte_buf
    }
}

fn seq<'de, V: Visitor<'de>>(de: De<'_>, items: &[Value], visitor: V) -> Result<V::Value, Error> {
    let mut access = Seq(de, items.iter().enumerate());
    let value = visitor.visit_seq(&mut access)?;
    let left = access.1.len();
    // The visitor must take every element: a tuple of fewer than the list holds is refused.
    if left > 0 {
        let expected = format!("{}", items.len() - left);
        return Err(Error::new(Message::Length {
            len: items.len(),
            expected,
        }));
    }
    Ok(value)
}

struct Seq<'v>(De<'v>, core::iter::Enumerate<core::slice::Iter<'v, Value>>);

impl<'de> SeqAccess<'de> for &mut Seq<'_> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        let Some((index, item)) = self.1.next() else {
            return Ok(None);
        };
        let element = seed.deserialize(self.0.of(item));
        element
            .map(Some)
            .map_err(|error| error.at(item.span, Token::Index(index)))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.1.len())
    }
}

struct Map<'v> {
    de: De<'v>,
    entries: core::slice::Iter<'v, Entry>,
    /// The entry whose key the visitor has read, the field's name, and whose value it reads next.
    entry: Option<(&'v str, &'v Entry)>,
}

impl<'v> Map<'v> {
    fn new(de: De<'v>, entries: &'v [Entry]) -> Self {
        Self {
            de,
            entries: entries.iter(),
            entry: None,
        }
    }
}

impl<'de> MapAccess<'de> for Map<'_> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        let Some(entry) = self.entries.next() else {
            return Ok(None);
        };
        let key = name(entry, "field")?;
        self.entry = Some((key, entry));
        let key = seed.deserialize(StrDeserializer::<Error>::new(key));
        key.map(Some).map_err(|error| error.at_key(entry.key_span))
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let Some((key, entry)) = self.entry.take() else {
            unreachable!(
                "a dict's value is asked for before its key, though `MapAccess` lets a visitor \
                 ask for a value only after its key"
            )
        };
        let value = seed.deserialize(self.de.of(&entry.value));
        value.map_err(|error| error.at(entry.value.span, Token::Key(String::from(key))))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.entries.len())
    }
}

fn name<'v>(entry: &'v Entry, what: &str) -> Result<&'v str, Error> {
    match &entry.key {
        Key::Str(key) => Ok(key),
        Key::Number(number) => {
            let message = format!(
                "`{}` names no {what}: name it with a string",
                number.literal()
            );
            Err(Error::new(Message::Whole(message)).at_key(entry.key_span))
        }
    }
}

/// A kind that a string names, which the kinds that hold a value can't be.
struct Named<'v>(&'v str);

impl<'de> EnumAccess<'de> for Named<'_> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Error> {
        let kind = seed.deserialize(StrDeserializer::<Error>::new(self.0))?;
        Ok((kind, self))
    }
}

impl Named<'_> {
    fn valueless(&self) -> Error {
        let kind = quote(self.0);
        let found = format!("`{kind}`");
        let expected = format!("a dict that gives the kind its value, as `{{{kind}: …}}`");
        Error::new(Message::Invalid { found, expected })
    }
}

impl<'de> VariantAccess<'de> for Named<'_> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _seed: T) -> Result<T::Value, Error> {
        Err(self.valueless())
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value, Error> {
        Err(self.valueless())
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Error> {
        Err(self.valueless())
    }
}

/// A kind that the key of a one-key dict names, and the value that the dict gives it.
struct Variant<'v> {
    de: De<'v>,
    kind: &'v str,
    entry: &'v Entry,
}

impl<'de> EnumAccess<'de> for Variant<'_> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), Error> {
        let kind = seed.deserialize(StrDeserializer::<Error>::new(self.kind));
        let kind = kind.map_err(|error| error.at_key(self.entry.key_span))?;
        Ok((kind, self))
    }
}

impl<'v> Variant<'v> {
    fn below(&self, error: Error) -> Error {
        error.at(self.entry.value.span, Token::Key(String::from(self.kind)))
    }

    fn value(&self) -> De<'v> {
        self.de.of(&self.entry.value)
    }
}

impl<'de> VariantAccess<'de> for Variant<'_> {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        <()>::deserialize(self.value()).map_err(|error| self.below(error))
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self.value())
            .map_err(|error| self.below(error))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_seq(self.value(), visitor).map_err(|error| self.below(error))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_map(self.value(), visitor).map_err(|error| self.below(error))
    }
}

// A caller's parameter annotated `annotation`, with `default`: none for an annotation that
// `--arg` can't read.
fn argument(annotation: Option<&Type>, default: Option<&Value>) -> Option<ir::Argument> {
    let argument = match (annotation?, default.map(|default| &default.kind)) {
        (Type::Str, None) => ir::Argument::Str { default: None },
        (Type::Str, Some(Kind::Str(text))) => ir::Argument::Str {
            default: Some(String::from(&**text)),
        },
        (Type::Int, None) => ir::Argument::Int { default: None },
        (Type::Int, Some(Kind::Int(int))) => ir::Argument::Int {
            default: Some(*int),
        },
        (Type::Bool, None) => ir::Argument::Bool { default: None },
        (Type::Bool, Some(Kind::Bool(bool))) => ir::Argument::Bool {
            default: Some(*bool),
        },
        (Type::None | Type::Float | Type::List(_) | Type::Dict(..) | Type::Hosted(_), _) => {
            return None;
        }
        (Type::Str | Type::Int | Type::Bool, Some(_)) => unreachable!(
            "a parameter's default misfits its annotation, though `define` refuses a default that \
             doesn't fit its annotation"
        ),
    };
    Some(argument)
}
