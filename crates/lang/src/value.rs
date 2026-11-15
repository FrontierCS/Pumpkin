//! Values, and the spans of source that make them.
//!
//! Every [`Value`] holds the [`Span`] that made it: a literal its own, the result of an operation
//! the operation's, and a value a builtin returns the call's. A value bound to a name keeps its
//! span wherever the name takes it, as does a value a function is given or returns, so a problem
//! with a value is reported where it was written.
//!
//! Values never change once made, since the language has no operation that changes one: so they
//! are shared, not copied, and the specification's freezing (spec.md, "Freezing a value") can't
//! be observed.
//!
//! A list, a tuple, a dict or a function nests at most `MAX_NESTING` deep, which [`Nested::new`]
//! and [`Function::new`] hold it to where it is made, so that no walk over a value, its drop
//! included, can overflow the stack.

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

/// A file an evaluation reads, by the order in which it read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId(pub(crate) usize);

impl FileId {
    pub(crate) const ENTRY: Self = Self(0);
}

/// A range of a file's text, in bytes, each end on a character's boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) file: FileId,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl Span {
    /// The span from this one's start to `last`'s end, in the same file.
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
    /// The specification's integers are unbounded; these refuse what overflows 64 bits.
    Int(i64),
    Str(Text),
    List(Nested<Value>),
    Tuple(Nested<Value>),
    Dict(Dict),
    /// A function a `def` made.
    Function(Rc<Function>),
    Builtin(Builtin),
    /// An image, which the IR names by its ID once built.
    Image(ImageId),
    Module(Module),
}

/// An image that `image()` or `codex()` declares, by its place among its evaluation's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageId(pub(crate) usize);

/// The bytes the values an evaluation has made take, never given back as values are dropped,
/// and the most they may.
/// The only constructors of a string, a list, a tuple, a dict and a function, [`Text::new`],
/// [`Nested::new`] and [`Function::new`], count what each makes: a string its length, a list's
/// or a tuple's element, or a function's default, `Value::BYTES`, a dict's entry `Entry::BYTES`.
/// Not counted: a string literal, which shares its file's text ([`Text::literal`]), and what
/// Pumpkin's functions plan, which is no value.
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

    /// Counts `bytes` more, of a value made at `at`, refused if the count passes the most.
    fn charge(&self, bytes: usize, at: Span) -> Result<(), Fault> {
        self.fits(bytes, at)?;
        self.used.set(self.used.get() + bytes);
        Ok(())
    }

    /// Refuses `bytes` more, of a value to be made at `at`, if they would pass the most: so
    /// that a value that may outgrow what makes it, as `replace`'s, is refused before it is made.
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

/// A string's text, made by [`Text::new`] or shared.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Text(Rc<str>);

impl Text {
    /// `text`, as the string made at `at`, counted in `budget`.
    pub(crate) fn new(text: &str, at: Span, budget: &Budget) -> Result<Self, Fault> {
        budget.charge(text.len(), at)?;
        Ok(Self(text.into()))
    }

    /// The text of a string literal, shared with its file's syntax.
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

/// The elements of a list, a tuple or a dict, which nest at most `MAX_NESTING` deep, it
/// counted.
#[derive(Clone)]
pub(crate) struct Nested<T> {
    elements: Rc<[T]>,
    /// One more than its deepest element's.
    depth: usize,
}

/// What a [`Nested`] holds: a value, or a dict's entry, which nests as deep as its value.
pub(crate) trait Element {
    /// The bytes [`Budget`] counts for it, at least what the assertion beside its implementation
    /// names.
    const BYTES: usize;

    /// How deep it nests: 0 for what holds no value.
    fn depth(&self) -> usize;
}

impl<T: Element> Nested<T> {
    /// `elements`, as those of the list, the tuple or the dict made at `at`, counted in
    /// `budget`, refused if one nests `MAX_NESTING` deep already.
    pub(crate) fn new(elements: Vec<T>, at: Span, budget: &Budget) -> Result<Self, Fault> {
        budget.charge(elements.len() * T::BYTES, at)?;
        let depth = deeper(elements.iter().map(Element::depth), at)?;
        let elements = elements.into();
        Ok(Self { elements, depth })
    }
}

/// One more than the deepest of `depths`, of what the value made at `at` holds, refused past
/// `MAX_NESTING`.
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
    /// This one's elements, then `other`'s, made at `at`, counted in `budget`.
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
    // What a value takes, and the counts of a string, which an element may be.
    const BYTES: usize = 80;

    fn depth(&self) -> usize {
        match &self.kind {
            Kind::None
            | Kind::Bool(_)
            | Kind::Int(_)
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
    // What an entry takes, the counts of a string, which its value may be, and its key and place
    // in the dict's index; and an estimate, not a bound, of its share of the index tree's nodes,
    // which the assertion below leaves out.
    const BYTES: usize = 160;

    fn depth(&self) -> usize {
        self.value.depth()
    }
}

/// The bytes of the counts an [`Rc`] keeps beside what it holds, such as a string's text.
const RC_COUNTS_BYTES: usize = 2 * size_of::<usize>();

const _: () = assert!(size_of::<Value>() + RC_COUNTS_BYTES <= <Value as Element>::BYTES);
const _: () = assert!(
    size_of::<Entry>() + RC_COUNTS_BYTES + size_of::<(Key, usize)>() <= <Entry as Element>::BYTES
);

/// A dict's entries, in the order their keys were first given, no two keys equal.
#[derive(Clone)]
pub(crate) struct Dict {
    entries: Nested<Entry>,
    /// Each key's place in `entries`.
    index: Rc<BTreeMap<Key, usize>>,
}

impl Dict {
    /// The dict of `entries`, made at `at`: each key at the place where it is first given, with
    /// the value, and the key's span, of the last entry that gives it. A value that nests
    /// `MAX_NESTING` deep already is refused.
    pub(crate) fn new(entries: Vec<Entry>, at: Span, budget: &Budget) -> Result<Self, Fault> {
        let (entries, index) = merged(entries);
        let entries = Nested::new(entries, at, budget)?;
        let index = Rc::new(index);
        Ok(Self { entries, index })
    }

    pub(crate) fn get(&self, key: &Key) -> Option<&Entry> {
        self.index.get(key).map(|&place| &self.entries[place])
    }

    /// This dict's entries, then `other`'s, whose values replace this one's for the keys that
    /// both have (spec.md, "Dictionaries"), made at `at`, counted in `budget`.
    pub(crate) fn union(&self, other: &Self, at: Span, budget: &Budget) -> Result<Self, Fault> {
        let entries = self.entries.iter().chain(other.entries.iter()).cloned();
        Self::new(entries.collect(), at, budget)
    }
}

/// `entries`, each key once, at its first place, with its last entry, and each key's place.
fn merged(entries: Vec<Entry>) -> (Vec<Entry>, BTreeMap<Key, usize>) {
    let mut index = BTreeMap::new();
    let mut merged: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(&place) = index.get(&entry.key) {
            merged[place] = entry;
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

/// A dict's entry: its key, with the span that made it, and its value.
#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) key: Key,
    pub(crate) key_span: Span,
    pub(crate) value: Value,
}

impl Entry {
    /// The key, as a value made where the key was.
    pub(crate) fn key_value(&self) -> Value {
        let kind = match &self.key {
            Key::Int(int) => Kind::Int(*int),
            Key::Str(text) => Kind::Str(text.clone()),
        };
        Value {
            kind,
            span: self.key_span,
        }
    }
}

/// A dict's key, which the subset holds to a string or an int, where the specification takes
/// any hashable value (spec.md, "Dictionaries").
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Int(i64),
    Str(Text),
}

/// Whether Starlark hashes `value`, which a dict's key must be.
fn hashable(value: &Value) -> bool {
    match &value.kind {
        Kind::Tuple(items) => items.iter().all(hashable),
        Kind::List(_) | Kind::Dict(_) | Kind::Image(_) | Kind::Module(_) => false,
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Str(_)
        | Kind::Function(_)
        | Kind::Builtin(_) => true,
    }
}

impl Key {
    /// The key that `value` is, if it is a string or an int; else the message that refuses it: a
    /// refusal of a value that the specification takes, a hashable one, and else Starlark's own
    /// error.
    pub(crate) fn of(value: &Value) -> Result<Self, String> {
        let what = value.kind.describe();
        let instead = "key a dict with strings or ints";
        match &value.kind {
            Kind::Int(int) => Ok(Self::Int(*int)),
            Kind::Str(text) => Ok(Self::Str(text.clone())),
            // Starlark hashes a tuple only if it hashes each of its elements.
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

    /// The key as a literal that makes it, for a message, a string [`quote`]d.
    pub(crate) fn literal(&self) -> String {
        match self {
            Self::Int(int) => format!("{int}"),
            Self::Str(text) => quote(text),
        }
    }
}

/// A function that a `def` made, which nests one level deeper than its deepest default.
pub(crate) struct Function {
    pub(crate) def: Rc<Def>,
    /// The file whose top level the body's names resolve in.
    pub(crate) module: FileId,
    /// Each parameter's default, in order, if it has one.
    pub(crate) defaults: Vec<Option<Value>>,
    depth: usize,
}

impl Function {
    /// The function that `def`, run at `module`'s top level, makes, with `defaults`, counted in
    /// `budget`, refused if a default nests `MAX_NESTING` deep already.
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

/// A function the language provides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Universal(Universal),
    Pumpkin(Pumpkin),
}

/// A module the language provides, whose functions a call names as the module's methods, as
/// `json.encode(…)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Module {
    Json,
}

impl Value {
    /// Whether the value is true as a condition (spec.md, "Booleans"): all but `None`, `False`,
    /// `0`, and empty strings, lists, tuples and dicts.
    pub(crate) fn truth(&self) -> bool {
        match &self.kind {
            Kind::None => false,
            Kind::Bool(bool) => *bool,
            Kind::Int(int) => *int != 0,
            Kind::Str(text) => !text.is_empty(),
            Kind::List(items) | Kind::Tuple(items) => !items.is_empty(),
            Kind::Dict(dict) => !dict.is_empty(),
            Kind::Function(_) | Kind::Builtin(_) | Kind::Image(_) | Kind::Module(_) => true,
        }
    }

    /// Whether the value equals `other`, as `==` has it (spec.md, "Comparisons"): two values of
    /// one kind with equal contents, a list never equalling a tuple, and a function only itself.
    pub(crate) fn equals(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (Kind::None, Kind::None) => true,
            (Kind::Bool(left), Kind::Bool(right)) => left == right,
            (Kind::Int(left), Kind::Int(right)) => left == right,
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
            (Kind::Function(left), Kind::Function(right)) => Rc::ptr_eq(&left.def, &right.def),
            (Kind::Builtin(left), Kind::Builtin(right)) => left == right,
            // Equal declarations are one image.
            (Kind::Image(left), Kind::Image(right)) => left == right,
            (Kind::Module(left), Kind::Module(right)) => left == right,
            (
                Kind::None
                | Kind::Bool(_)
                | Kind::Int(_)
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

    /// The value, for a message: `None`, a bool or an int as its literal, a string [`quote`]d,
    /// any other as its kind.
    pub(crate) fn shown(&self) -> String {
        match &self.kind {
            Kind::None => String::from("`None`"),
            Kind::Bool(true) => String::from("`True`"),
            Kind::Bool(false) => String::from("`False`"),
            Kind::Int(int) => format!("`{int}`"),
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
    /// What kind of value this is, for a message: "an int", "a dict".
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::None => "`None`",
            Self::Bool(_) => "a bool",
            Self::Int(_) => "an int",
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

/// How many characters of a string a message shows, at most.
// So that a message stays short, however long the strings it shows.
const SHOWN: usize = 60;

/// `text`, for a message: whole, or its first `SHOWN` characters, then `…`.
fn cut(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(SHOWN) {
        Some((end, _)) => Cow::Owned(format!("{}…", &text[..end])),
        None => Cow::Borrowed(text),
    }
}

/// `text`, for a message, `cut`, with each `"`, `\`, control character, line or paragraph
/// separator and character of Unicode's `Bidi_Control` property written as the language's
/// escape in a string literal in double quotes, or, where it has none, as `<U+XXXX>`.
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
            // A renderer may break a message's line at U+2028 or U+2029, the line and paragraph
            // separators, as at a newline, and a bidi control reorders how the text around it
            // reads. The language escapes an ASCII one in octal, and no other.
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

/// `text`, for a message, as a Starlark string literal in double quotes, [`escape`]d.
pub(crate) fn quote(text: &str) -> String {
    format!("\"{}\"", escape(text))
}
