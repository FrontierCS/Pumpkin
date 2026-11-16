//! The values an evaluation makes, each with the span of the code that made it, and the budget
//! that bounds their bytes.
//!
//! Every [`Value`] holds the [`Span`] that made it, and keeps it wherever the value goes, so that
//! a problem with a value is reported where the value was written. Values never change once made,
//! so they are shared, not copied, and the specification's freezing (spec.md, "Freezing a value")
//! can't be observed.
//!
//! A string, list, tuple, dict or function is made only through a constructor that charges the
//! `Budget` and checks its nesting against `MAX_NESTING`, so that no value outgrows the budget,
//! and no walk over a value, its drop included, can overflow the stack.

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::fmt::Write;
use core::ops::Deref;

use crate::MAX_NESTING;
use crate::builtins::Universal;
use crate::problem::{Fault, refusal};
use crate::pumpkin::Pumpkin;
use crate::syntax::Def;

mod number;

pub(crate) use number::{Number, float_of, float_text, floor, modulo, truncated};

/// A file an evaluation reads, by the order in which it read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId(pub(crate) usize);

impl FileId {
    pub(crate) const ENTRY: Self = Self(0);
}

/// A range of one file's text, in bytes, each end on a character's boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) file: FileId,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl Span {
    /// The span from the start of `self` to the end of `last`, which is in the same file.
    pub(crate) fn to(self, last: Self) -> Self {
        Self {
            end: last.end,
            ..self
        }
    }
}

#[derive(Clone)]
pub(crate) struct Value {
    pub(crate) kind: Kind,
    pub(crate) span: Span,
}

#[derive(Clone)]
pub(crate) enum Kind {
    None,
    Bool(bool),
    /// 64 bits, where the specification's ints are unbounded.
    Int(i64),
    Float(f64),
    Str(Text),
    List(Nested<Value>),
    Tuple(Nested<Value>),
    Dict(Dict),
    Function(Rc<Function>),
    Builtin(Builtin),
    Image(ImageId),
    Module(Module),
}

/// An image that `image()` declares, by its place among its evaluation's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageId(pub(crate) usize);

/// The bytes that an evaluation's values may take in all. Each value is charged once, when it is
/// made, and nothing is refunded when it is dropped.
pub(crate) struct Budget {
    used: Cell<usize>,
    max: usize,
}

impl Budget {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            used: Cell::new(0),
            max,
        }
    }

    fn charge(&self, bytes: usize, at: Span) -> Result<(), Fault> {
        self.fits(bytes, at)?;
        self.used.set(self.used.get() + bytes);
        Ok(())
    }

    /// Checks that `bytes` more fit, and charges none. A caller about to build a large value
    /// calls it first, so that the budget refuses before the allocation.
    pub(crate) fn fits(&self, bytes: usize, at: Span) -> Result<(), Fault> {
        if bytes <= self.max - self.used.get() {
            return Ok(());
        }
        let message = format!(
            "this evaluation's strings, lists, tuples and dicts take more than {} bytes: make \
             smaller values, as with fewer `+` of long ones",
            self.max
        );
        Err(Fault::new(at, message))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Text(Rc<str>);

impl Text {
    pub(crate) fn new(text: &str, at: Span, budget: &Budget) -> Result<Self, Fault> {
        budget.charge(text.len(), at)?;
        Ok(Self(text.into()))
    }

    /// Shares the text of a literal in the syntax tree, which the file's size already bounds, so
    /// it charges nothing.
    pub(crate) fn literal(text: &Rc<str>) -> Self {
        Self(Rc::clone(text))
    }
}

impl Deref for Text {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

/// The elements of a list, tuple or dict, shared between clones, which charge nothing. `depth`
/// is one more than the deepest element's.
#[derive(Clone)]
pub(crate) struct Nested<T> {
    elements: Rc<[T]>,
    depth: usize,
}

pub(crate) trait Element {
    /// What one element costs the budget: an estimate of its bytes on the heap, at least what
    /// the assertions below count.
    const BYTES: usize;

    fn depth(&self) -> usize;
}

impl<T: Element> Nested<T> {
    pub(crate) fn new(elements: Vec<T>, at: Span, budget: &Budget) -> Result<Self, Fault> {
        budget.charge(elements.len() * T::BYTES, at)?;
        let depth = deeper(elements.iter().map(Element::depth), at)?;
        let elements = elements.into();
        Ok(Self { elements, depth })
    }
}

fn deeper(depths: impl Iterator<Item = usize>, at: Span) -> Result<usize, Fault> {
    let depth = 1 + depths.max().unwrap_or(0);
    if depth <= MAX_NESTING {
        return Ok(depth);
    }
    let message = format!(
        "this makes a value nested {depth} deep: nest lists, tuples, dicts and functions' \
         defaults at most {MAX_NESTING} deep"
    );
    Err(Fault::new(at, message))
}

impl<T: Clone + Element> Nested<T> {
    pub(crate) fn concat(&self, other: &Self, at: Span, budget: &Budget) -> Result<Self, Fault> {
        let elements = self.elements.iter().chain(other.elements.iter());
        Self::new(elements.cloned().collect(), at, budget)
    }
}

impl<T> Deref for Nested<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.elements
    }
}

impl Element for Value {
    const BYTES: usize = 80;

    fn depth(&self) -> usize {
        match &self.kind {
            Kind::None
            | Kind::Bool(_)
            | Kind::Int(_)
            | Kind::Float(_)
            | Kind::Str(_)
            | Kind::Builtin(_)
            | Kind::Image(_)
            | Kind::Module(_) => 0,
            Kind::List(elements) | Kind::Tuple(elements) => elements.depth,
            Kind::Dict(dict) => dict.entries.depth,
            Kind::Function(function) => function.depth,
        }
    }
}

impl Element for Entry {
    const BYTES: usize = 160;

    fn depth(&self) -> usize {
        self.value.depth()
    }
}

// What the assertions count of an element: its own bytes, its `Rc`'s strong and weak counts and,
// for a dict entry, its key and place in the index. An entry's `BYTES` also covers its share of
// the index tree's nodes, which no assertion can count.
const RC_COUNTS_BYTES: usize = 2 * size_of::<usize>();

const _: () = assert!(size_of::<Value>() + RC_COUNTS_BYTES <= <Value as Element>::BYTES);
const _: () = assert!(
    size_of::<Entry>() + RC_COUNTS_BYTES + size_of::<(Key, usize)>() <= <Entry as Element>::BYTES
);

/// A dict in insertion order. `index` gives each key's place in `entries`.
#[derive(Clone)]
pub(crate) struct Dict {
    entries: Nested<Entry>,
    index: Rc<BTreeMap<Key, usize>>,
}

impl Dict {
    /// A key that repeats keeps its first form and place, and its last value.
    pub(crate) fn new(entries: Vec<Entry>, at: Span, budget: &Budget) -> Result<Self, Fault> {
        let (entries, index) = merged(entries);
        let entries = Nested::new(entries, at, budget)?;
        let index = Rc::new(index);
        Ok(Self { entries, index })
    }

    pub(crate) fn get(&self, key: &Key) -> Option<&Entry> {
        self.index.get(key).map(|&place| &self.entries[place])
    }

    /// The entries of `self`, then those of `other`, where a key in both keeps its place in
    /// `self` and its value in `other`.
    pub(crate) fn union(&self, other: &Self, at: Span, budget: &Budget) -> Result<Self, Fault> {
        let entries = self.entries.iter().chain(other.entries.iter()).cloned();
        Self::new(entries.collect(), at, budget)
    }
}

fn merged(entries: Vec<Entry>) -> (Vec<Entry>, BTreeMap<Key, usize>) {
    let mut index: BTreeMap<Key, usize> = BTreeMap::new();
    let mut merged: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(&place) = index.get(&entry.key) {
            merged[place].value = entry.value;
        } else {
            index.insert(entry.key.clone(), merged.len());
            merged.push(entry);
        }
    }
    (merged, index)
}

impl Deref for Dict {
    type Target = [Entry];

    fn deref(&self) -> &[Entry] {
        &self.entries
    }
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) key: Key,
    pub(crate) key_span: Span,
    pub(crate) value: Value,
}

impl Entry {
    pub(crate) fn key_value(&self) -> Value {
        let kind = match &self.key {
            Key::Number(number) => number.kind(),
            Key::Str(text) => Kind::Str(text.clone()),
        };
        Value {
            kind,
            span: self.key_span,
        }
    }
}

/// A dict's key, which the subset holds to a string or a number, where the specification takes
/// any hashable value (spec.md, "Dictionaries"). Equal numbers are one key, so `1` finds `1.0`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Number(Number),
    Str(Text),
}

fn hashable(value: &Value) -> bool {
    match &value.kind {
        Kind::Tuple(items) => items.iter().all(hashable),
        Kind::List(_) | Kind::Dict(_) | Kind::Image(_) | Kind::Module(_) => false,
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Float(_)
        | Kind::Str(_)
        | Kind::Function(_)
        | Kind::Builtin(_) => true,
    }
}

impl Key {
    pub(crate) fn of(value: &Value) -> Result<Self, String> {
        let what = value.kind.describe();
        let instead = "key a dict with strings or numbers";
        match &value.kind {
            Kind::Int(int) => Ok(Self::Number(Number::Int(*int))),
            Kind::Float(float) => Ok(Self::Number(Number::Float(*float))),
            Kind::Str(text) => Ok(Self::Str(text.clone())),
            Kind::Tuple(items) if !items.iter().all(hashable) => Err(format!(
                "a tuple that holds an unhashable value can't be a dict's key: {instead}"
            )),
            Kind::None | Kind::Bool(_) | Kind::Tuple(_) | Kind::Function(_) | Kind::Builtin(_) => {
                Err(refusal(format_args!("{what} as a dict's key"), instead))
            }
            Kind::List(_) | Kind::Dict(_) | Kind::Image(_) | Kind::Module(_) => {
                Err(format!("{what} can't be a dict's key: {instead}"))
            }
        }
    }

    pub(crate) fn literal(&self) -> String {
        match self {
            Self::Number(number) => number.literal(),
            Self::Str(text) => quote(text),
        }
    }
}

/// A function made by a `def`, with the module whose globals it reads and the values of its
/// parameters' defaults, `None` for a parameter without one.
pub(crate) struct Function {
    pub(crate) def: Rc<Def>,
    pub(crate) module: FileId,
    pub(crate) defaults: Vec<Option<Value>>,
    depth: usize,
}

impl Function {
    pub(crate) fn new(
        def: Rc<Def>,
        module: FileId,
        defaults: Vec<Option<Value>>,
        budget: &Budget,
    ) -> Result<Self, Fault> {
        budget.charge(defaults.len() * Value::BYTES, def.name.span)?;
        let depths = defaults.iter().flatten().map(Element::depth);
        let depth = deeper(depths, def.name.span)?;
        Ok(Self {
            def,
            module,
            defaults,
            depth,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Universal(Universal),
    Pumpkin(Pumpkin),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Module {
    Json,
}

impl Value {
    pub(crate) fn truth(&self) -> bool {
        match &self.kind {
            Kind::None => false,
            Kind::Bool(bool) => *bool,
            Kind::Int(int) => *int != 0,
            Kind::Float(float) => *float != 0.0,
            Kind::Str(text) => !text.is_empty(),
            Kind::List(items) | Kind::Tuple(items) => !items.is_empty(),
            Kind::Dict(dict) => !dict.is_empty(),
            Kind::Function(_) | Kind::Builtin(_) | Kind::Image(_) | Kind::Module(_) => true,
        }
    }

    pub(crate) fn equals(&self, other: &Self) -> bool {
        if let (Some(left), Some(right)) = (Number::of(&self.kind), Number::of(&other.kind)) {
            return left == right;
        }
        match (&self.kind, &other.kind) {
            (Kind::None, Kind::None) => true,
            (Kind::Bool(left), Kind::Bool(right)) => left == right,
            (Kind::Str(left), Kind::Str(right)) => left == right,
            (Kind::List(left), Kind::List(right)) | (Kind::Tuple(left), Kind::Tuple(right)) => {
                left.len() == right.len()
                    && (left.iter().zip(right.iter())).all(|(left, right)| left.equals(right))
            }
            (Kind::Dict(left), Kind::Dict(right)) => {
                left.len() == right.len()
                    && left.iter().all(|entry| {
                        (right.get(&entry.key))
                            .is_some_and(|other| other.value.equals(&entry.value))
                    })
            }
            // A `def` runs once, and so makes one function, which equals only itself.
            (Kind::Function(left), Kind::Function(right)) => Rc::ptr_eq(&left.def, &right.def),
            (Kind::Builtin(left), Kind::Builtin(right)) => left == right,
            (Kind::Image(left), Kind::Image(right)) => left == right,
            (Kind::Module(left), Kind::Module(right)) => left == right,
            (
                Kind::None
                | Kind::Bool(_)
                | Kind::Int(_)
                | Kind::Float(_)
                | Kind::Str(_)
                | Kind::List(_)
                | Kind::Tuple(_)
                | Kind::Dict(_)
                | Kind::Function(_)
                | Kind::Builtin(_)
                | Kind::Image(_)
                | Kind::Module(_),
                _,
            ) => false,
        }
    }

    /// How a message names this value: `None`, a bool, a number or a string by its literal, any
    /// other by its type.
    pub(crate) fn shown(&self) -> String {
        match &self.kind {
            Kind::None => String::from("`None`"),
            Kind::Bool(true) => String::from("`True`"),
            Kind::Bool(false) => String::from("`False`"),
            Kind::Int(int) => format!("`{int}`"),
            Kind::Float(float) => format!("`{}`", float_text(*float)),
            Kind::Str(text) => format!("`{}`", quote(text)),
            Kind::List(_)
            | Kind::Tuple(_)
            | Kind::Dict(_)
            | Kind::Function(_)
            | Kind::Builtin(_)
            | Kind::Image(_)
            | Kind::Module(_) => String::from(self.kind.describe()),
        }
    }
}

impl Kind {
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::None => "`None`",
            Self::Bool(_) => "a bool",
            Self::Int(_) => "an int",
            Self::Float(_) => "a float",
            Self::Str(_) => "a string",
            Self::List(_) => "a list",
            Self::Tuple(_) => "a tuple",
            Self::Dict(_) => "a dict",
            Self::Function(_) | Self::Builtin(_) => "a function",
            Self::Image(_) => "an image",
            Self::Module(Module::Json) => "the module `json`",
        }
    }
}

// So that a long string can't bury the rest of a message.
const SHOWN: usize = 60;

fn cut(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(SHOWN) {
        Some((end, _)) => Cow::Owned(format!("{}…", &text[..end])),
        None => Cow::Borrowed(text),
    }
}

/// Makes `text` safe to put in a message: cut to `SHOWN` characters, with quotes, backslashes,
/// control characters, line and paragraph separators and bidirectional controls written as the
/// language's escape in a double-quoted string, or as `<U+XXXX>` where it has none.
pub(crate) fn escape(text: &str) -> String {
    let mut escaped = String::new();
    for char in cut(text).chars() {
        match char {
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(char);
            }
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            // A renderer may break a line of a message at U+2028 or U+2029, as at a newline, and a
            // bidirectional control reorders the text around it. The language has an escape for
            // an ASCII character alone, in octal.
            char if char.is_control()
                || matches!(
                    char,
                    '\u{061c}'
                        | '\u{200e}'..='\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                ) =>
            {
                // Writing to a `String` can't fail.
                let _ = if char.is_ascii() {
                    write!(escaped, "\\{:03o}", u32::from(char))
                } else {
                    write!(escaped, "<U+{:04X}>", u32::from(char))
                };
            }
            char => escaped.push(char),
        }
    }
    escaped
}

pub(crate) fn quote(text: &str) -> String {
    format!("\"{}\"", escape(text))
}
