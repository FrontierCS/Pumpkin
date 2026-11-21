//! A file's syntax: its text read into a [`Module`], whose names are checked before it runs.
//!
//! The grammar is the specification's (spec.md, "Lexical elements", "Expressions" and
//! "Statements"), cut down to these:
//! - at a file's top level: `load`, assignment to a name or to names in a tuple or list, `pass`,
//!   expression statements, which may be separated by `;`, and `def`, but in an entry file;
//! - in a function's body: the same but `load` and `def`, and `return` and `if`, `elif` and
//!   `else`;
//! - parameters: positional ones, then those after a bare `*`, which are keyword-only, each with
//!   a default or without, but no positional one without a default after one with;
//! - expressions: names, ints, floats, strings, lists, tuples, dicts, list and dict comprehensions,
//!   parentheses, calls with positional then keyword arguments, then a `**` dict of keyword ones,
//!   methods called where they are named, indexing, `a if c else b`, `or`, `and`, `not`,
//!   comparisons, which don't chain, `in`, `not in`, `|`, `+`, `-`, `*`, `/`, `//`, `%` and unary
//!   `-`.
//!
//! It adds the annotations that the crate's doc names: each parameter with a type after `:` or
//! without, as is a `def`'s return after `->`, a type being `None`, `bool`, `int`, `float`,
//! `str`, `list[T]` or `dict[K, V]`, whose `K` is `int`, `float` or `str`, or the host's
//! `Target`, `Runner`, `Run` or `Payload`.
//!
//! Each place where the subset is narrower than the specification is refused at its token, and
//! the rule of the refusal is written where it is checked: the lexical ones in `lex`, the
//! grammar's in `parse`, with the nesting cap, `MAX_NESTING`, and the names' in `resolve`. Each
//! node that a message can point at holds its [`Span`].

mod lex;
mod parse;
mod resolve;

use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

pub(crate) use lex::is_name;
use lex::{Keyword, Punct};
pub(crate) use parse::parse;
pub(crate) use resolve::resolve;

use crate::Label;
use crate::value::{Hosted, Key, Kind, Number, Span, Value};

/// A file's statements: its loads, and the others, each in the order written.
pub(crate) struct Module {
    pub(crate) loads: Vec<Load>,
    pub(crate) statements: Vec<Statement>,
}

impl Module {
    /// Each name the top level binds, by an assignment or a `def`, in order, as often as bound.
    pub(crate) fn globals(&self) -> Vec<&Name> {
        let mut names = Vec::new();
        for statement in &self.statements {
            match statement {
                Statement::Simple(Simple::Assign { target, .. }) => target.names(&mut names),
                Statement::Simple(Simple::Expr(_) | Simple::Pass(_)) => {}
                Statement::Def(def) => names.push(&def.name),
            }
        }
        names
    }
}

/// A statement at a file's top level, other than `load`.
pub(crate) enum Statement {
    Simple(Simple),
    /// Shared with the function it defines, which runs it.
    Def(Rc<Def>),
}

impl Statement {
    pub(crate) fn span(&self) -> Span {
        match self {
            Self::Simple(simple) => simple.span(),
            Self::Def(def) => def.keyword,
        }
    }
}

pub(crate) enum Simple {
    Assign { target: Target, value: Expr },
    Expr(Expr),
    Pass(Span),
}

impl Simple {
    fn span(&self) -> Span {
        match self {
            Self::Assign { target, .. } => target.span(),
            Self::Expr(expr) => expr.span,
            Self::Pass(span) => *span,
        }
    }
}

pub(crate) enum Step {
    Simple(Simple),
    Return {
        keyword: Span,
        value: Option<Expr>,
    },
    /// The `if` and each `elif` in order, and the `else`'s steps, none without an `else`.
    If {
        branches: Vec<Branch>,
        otherwise: Vec<Self>,
    },
}

pub(crate) struct Branch {
    pub(crate) condition: Expr,
    pub(crate) steps: Vec<Step>,
}

pub(crate) struct Def {
    pub(crate) keyword: Span,
    pub(crate) name: Name,
    /// No two named alike.
    pub(crate) params: Vec<Param>,
    /// How many of `params` may be given by position: those before a bare `*`, or all.
    pub(crate) positional: usize,
    pub(crate) returns: Option<Type>,
    pub(crate) body: Vec<Step>,
    /// Each name the function binds, once: its parameters, then the names its body assigns.
    pub(crate) locals: Vec<String>,
    /// How deep the body nests in its file, its block counted.
    pub(crate) depth: usize,
}

pub(crate) struct Param {
    pub(crate) name: Name,
    pub(crate) annotation: Option<Type>,
    pub(crate) default: Option<Expr>,
}

/// A type that an annotation names.
#[derive(PartialEq)]
pub(crate) enum Type {
    None,
    Bool,
    Int,
    Float,
    Str,
    List(Box<Self>),
    Dict(KeyType, Box<Self>),
    Hosted(Hosted),
}

/// The type of a dict's keys, which can only be what a key can be: a number or a string.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum KeyType {
    Int,
    Float,
    Str,
}

/// Where a value doesn't fit a type.
pub(crate) enum Misfit {
    /// The value itself is not of the type.
    Whole,
    Inside(Inside),
}

/// A value that a list or a dict holds, or a dict's key, that is not of its own type, found
/// inside the value checked.
pub(crate) struct Inside {
    /// The value, or the key made a value.
    pub(crate) found: Value,
    /// How a message names a value of the type that `found` is not of.
    pub(crate) fits: String,
    /// What `found` is to the list or the dict that holds it.
    pub(crate) part: Part,
    /// What that list or dict is to the one that holds it, and so on out to the value checked.
    pub(crate) outer: Vec<Part>,
}

/// What a value is to the list or the dict that holds it.
#[derive(Clone, Copy)]
pub(crate) enum Part {
    Element,
    Key,
    Value,
}

impl Type {
    /// Where `value` doesn't fit this type, or else `None`: a value fits by its kind exactly, so
    /// that an int is no float and a bool no int, and a list or a dict where each element, key
    /// and value fits its own type, as starlark-rust's matchers check:
    /// <https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/typing/type_compiled/matchers.rs#L92-L206>.
    /// The first that doesn't fit is the one found.
    pub(crate) fn misfit(&self, value: &Value) -> Option<Misfit> {
        match (self, &value.kind) {
            (Self::None, Kind::None)
            | (Self::Bool, Kind::Bool(_))
            | (Self::Int, Kind::Int(_))
            | (Self::Float, Kind::Float(_))
            | (Self::Str, Kind::Str(_)) => None,
            (Self::Hosted(hosted), Kind::Hosted(made)) => {
                (made.ty() != *hosted).then_some(Misfit::Whole)
            }
            (Self::List(element), Kind::List(elements)) => {
                (elements.iter()).find_map(|value| element.held(value, Part::Element))
            }
            (Self::Dict(key, value), Kind::Dict(dict)) => dict.iter().find_map(|entry| {
                if !key.fits(&entry.key) {
                    return Some(Misfit::Inside(Inside {
                        found: entry.key_value(),
                        fits: key.as_type().describe(),
                        part: Part::Key,
                        outer: Vec::new(),
                    }));
                }
                value.held(&entry.value, Part::Value)
            }),
            (
                Self::None
                | Self::Bool
                | Self::Int
                | Self::Float
                | Self::Str
                | Self::List(_)
                | Self::Dict(..)
                | Self::Hosted(_),
                _,
            ) => Some(Misfit::Whole),
        }
    }

    // Where `value`, which a list or a dict holds as `part`, doesn't fit this type, as a misfit
    // inside that list or dict.
    fn held(&self, value: &Value, part: Part) -> Option<Misfit> {
        let inside = match self.misfit(value)? {
            Misfit::Whole => Inside {
                found: value.clone(),
                fits: self.describe(),
                part,
                outer: Vec::new(),
            },
            Misfit::Inside(mut inside) => {
                inside.outer.push(part);
                inside
            }
        };
        Some(Misfit::Inside(inside))
    }

    /// How a message names a value of this type, as `Kind::describe` names a value's kind.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::None => String::from("`None`"),
            Self::Bool => String::from("a bool"),
            Self::Int => String::from("an int"),
            Self::Float => String::from("a float"),
            Self::Str => String::from("a string"),
            Self::List(element) => format!("a list of {}", element.plural()),
            Self::Dict(key, value) => {
                format!("a dict of {} to {}", key.as_type().plural(), value.plural())
            }
            Self::Hosted(hosted) => String::from(hosted.describe()),
        }
    }

    // How `describe` names values of this type that a list or a dict holds.
    fn plural(&self) -> String {
        match self {
            Self::None => String::from("`None`"),
            Self::Bool => String::from("bools"),
            Self::Int => String::from("ints"),
            Self::Float => String::from("floats"),
            Self::Str => String::from("strings"),
            Self::List(element) => format!("lists of {}", element.plural()),
            Self::Dict(key, value) => {
                format!("dicts of {} to {}", key.as_type().plural(), value.plural())
            }
            Self::Hosted(Hosted::Target) => String::from("`Target`s"),
            Self::Hosted(Hosted::Runner(())) => String::from("`Runner`s"),
            Self::Hosted(Hosted::Run) => String::from("`Run`s"),
            Self::Hosted(Hosted::Payload(())) => String::from("payloads"),
        }
    }
}

impl KeyType {
    fn fits(self, key: &Key) -> bool {
        match (self, key) {
            (Self::Int, Key::Number(Number::Int(_)))
            | (Self::Float, Key::Number(Number::Float(_)))
            | (Self::Str, Key::Str(_)) => true,
            (Self::Int | Self::Float | Self::Str, _) => false,
        }
    }

    // The type of a value that a key of this type is, which messages name.
    fn as_type(self) -> Type {
        match self {
            Self::Int => Type::Int,
            Self::Float => Type::Float,
            Self::Str => Type::Str,
        }
    }
}

impl Part {
    /// How a message names a value that is this part of another, as in "an element of `xs`".
    pub(crate) fn of(self) -> &'static str {
        match self {
            Self::Element => "an element of",
            Self::Key => "a key of",
            Self::Value => "a value of",
        }
    }

    /// How a note names a value that is this part of another, as in "the element, `1`, is made
    /// at".
    pub(crate) fn the(self) -> &'static str {
        match self {
            Self::Element => "the element",
            Self::Key => "the key",
            Self::Value => "the value",
        }
    }
}

pub(crate) struct Load {
    pub(crate) keyword: Span,
    pub(crate) label: Label,
    /// The label's string literal.
    pub(crate) at: Span,
    pub(crate) names: Vec<LoadName>,
}

/// A name that a `load` binds: `local` is the name in this file, and `remote` the global it
/// takes from the loaded file; one name when the `load` gives no alias.
pub(crate) struct LoadName {
    pub(crate) local: Name,
    /// The string literal that names the global, which may name no valid one.
    pub(crate) remote: Name,
}

pub(crate) struct Name {
    pub(crate) text: String,
    pub(crate) span: Span,
}

/// What an assignment, or a comprehension's `for`, binds.
pub(crate) enum Target {
    Name(Name),
    /// Names in a tuple or a list, which take the elements of a list or tuple as long.
    Each {
        targets: Vec<Self>,
        span: Span,
    },
}

impl Target {
    pub(crate) fn span(&self) -> Span {
        match self {
            Self::Name(name) => name.span,
            Self::Each { span, .. } => *span,
        }
    }

    pub(crate) fn names<'t>(&'t self, names: &mut Vec<&'t Name>) {
        match self {
            Self::Name(name) => names.push(name),
            Self::Each { targets, .. } => {
                for target in targets {
                    target.names(names);
                }
            }
        }
    }
}

pub(crate) struct Expr {
    pub(crate) kind: ExprKind,
    pub(crate) span: Span,
}

// Each variant is small, its parts boxed where they are larger, since expressions are on the
// parser's and the evaluator's deepest paths.
pub(crate) enum ExprKind {
    Name(String),
    Int(i64),
    Float(f64),
    Str(Rc<str>),
    List(Vec<Expr>),
    Tuple(Vec<Expr>),
    Dict(Vec<(Expr, Expr)>),
    ListComp {
        body: Box<Expr>,
        comprehension: Box<Comprehension>,
    },
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        comprehension: Box<Comprehension>,
    },
    Call {
        callee: Box<Expr>,
        args: Box<Args>,
        /// How deep the call's arguments nest in its file, its `(` counted.
        depth: usize,
    },
    Method {
        receiver: Box<Expr>,
        depth: usize,
        name: Box<Name>,
        args: Box<Args>,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    Unary {
        operator: Unary,
        at: Span,
        operand: Box<Expr>,
    },
    /// `first` joined to each of `rest` by `operator`, which is the same all along.
    Logic {
        operator: Logic,
        first: Box<Expr>,
        rest: Vec<Expr>,
    },
    /// A chain of binary operators of one precedence, each with its span and its right operand.
    /// A comparison, which doesn't associate, is a chain of one.
    Binary {
        first: Box<Expr>,
        rest: Vec<(Operator, Span, Expr)>,
    },
    /// `then if condition else otherwise`.
    Conditional {
        then: Box<Expr>,
        condition: Box<Expr>,
        otherwise: Box<Expr>,
    },
}

pub(crate) struct Args {
    pub(crate) positional: Vec<Expr>,
    pub(crate) keyword: Vec<(Name, Expr)>,
    /// The dict that `**` passes, whose entries are keyword arguments after `keyword`.
    pub(crate) kwargs: Option<Expr>,
}

/// A comprehension's clauses: a first `for`, whose iterable is read in the block around the
/// comprehension, then the rest, read in the comprehension's own.
pub(crate) struct Comprehension {
    pub(crate) target: Target,
    pub(crate) iterable: Box<Expr>,
    pub(crate) clauses: Vec<Clause>,
}

impl Comprehension {
    pub(crate) fn names(&self) -> Vec<&Name> {
        let mut names = Vec::new();
        self.target.names(&mut names);
        for clause in &self.clauses {
            match clause {
                Clause::For { target, .. } => target.names(&mut names),
                Clause::If(_) => {}
            }
        }
        names
    }
}

pub(crate) enum Clause {
    For { target: Target, iterable: Expr },
    If(Expr),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Unary {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Logic {
    And,
    Or,
}

impl Logic {
    pub(crate) fn keyword(self) -> Keyword {
        match self {
            Self::And => Keyword::And,
            Self::Or => Keyword::Or,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Operator {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    NotIn,
    Union,
    Arithmetic(Arithmetic),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Arithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
    FloorDivide,
    Modulo,
}

impl Operator {
    pub(crate) fn symbol(self) -> &'static str {
        let punct = match self {
            Self::Equal => Punct::EqEq,
            Self::NotEqual => Punct::BangEq,
            Self::Less => Punct::Less,
            Self::LessEqual => Punct::LessEq,
            Self::Greater => Punct::Greater,
            Self::GreaterEqual => Punct::GreaterEq,
            Self::In => return Keyword::In.text(),
            Self::NotIn => return "not in",
            Self::Union => Punct::Pipe,
            Self::Arithmetic(arithmetic) => return arithmetic.symbol(),
        };
        punct.text()
    }
}

impl Arithmetic {
    pub(crate) fn symbol(self) -> &'static str {
        let punct = match self {
            Self::Add => Punct::Plus,
            Self::Subtract => Punct::Minus,
            Self::Multiply => Punct::Star,
            Self::Divide => Punct::Slash,
            Self::FloorDivide => Punct::SlashSlash,
            Self::Modulo => Punct::Percent,
        };
        punct.text()
    }
}
