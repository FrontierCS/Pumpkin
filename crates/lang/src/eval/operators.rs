//! What operators make of their operands (spec.md, "Expressions"), but for what the subset
//! refuses:
//! - `+` joins two strings, two lists or two tuples, and `*` and `%` take two ints, since the
//!   subset repeats no sequence and formats no string;
//! - an int operation that overflows 64 bits is refused, and one that divides by zero is an
//!   error, at its operator;
//!   `//` and `%` floor ("Arithmetic operations");
//! - `<`, `<=`, `>` and `>=` order two ints or two strings, a string by its UTF-8 bytes, as
//!   Starlark's implementations in Go and in Rust do, and no bools, lists or tuples;
//! - `|` merges two dicts ("Dictionaries"), and ORs no ints, as the subset has no bitwise
//!   operators;
//! - `in` finds an element of a list or a tuple, a key, a string or an int, of a dict, or a
//!   string in a string ("Membership tests");
//! - an index of a list or a tuple of n is an int from -n to n - 1, and a dict's a key; and a
//!   comprehension iterates over a list, a tuple or a dict's keys. A string is not indexed, as
//!   Starlark's implementations count its elements differently.
//!
//! An error or a refusal of an operand's kind names where the operand was made.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use crate::builtins::origin;
use crate::problem::{Fault, refusal};
use crate::syntax::{Arithmetic, Operator};
use crate::value::{Budget, Entry, Key, Kind, Span, Text, Value};

pub(super) fn binary(
    operator: Operator,
    at: Span,
    left: &Value,
    right: &Value,
    budget: &Budget,
) -> Result<Kind, Fault> {
    let (symbol, operands) = (operator.symbol(), (left, right));
    let ordered = |test: fn(Ordering) -> bool| order(symbol, at, operands, test).map(Kind::Bool);
    match operator {
        Operator::Equal => Ok(Kind::Bool(left.equals(right))),
        Operator::NotEqual => Ok(Kind::Bool(!left.equals(right))),
        Operator::Less => ordered(Ordering::is_lt),
        Operator::LessEqual => ordered(Ordering::is_le),
        Operator::Greater => ordered(Ordering::is_gt),
        Operator::GreaterEqual => ordered(Ordering::is_ge),
        Operator::In => contains(symbol, at, operands).map(Kind::Bool),
        Operator::NotIn => contains(symbol, at, operands).map(|found| Kind::Bool(!found)),
        Operator::Union => union(at, operands, budget),
        Operator::Arithmetic(arithmetic) => self::arithmetic(arithmetic, at, operands, budget),
    }
}

fn rejected(symbol: &str, at: Span, operands: (&Value, &Value), takes: &str) -> Fault {
    let (left, right) = operands;
    let message = format!(
        "`{symbol}` can't take {} and {}: give it {takes}",
        left.kind.describe(),
        right.kind.describe()
    );
    let fault = origin(Fault::new(at, message), "the left operand", left);
    origin(fault, "the right operand", right)
}

fn order(
    symbol: &str,
    at: Span,
    operands: (&Value, &Value),
    test: fn(Ordering) -> bool,
) -> Result<bool, Fault> {
    // Starlark orders two bools, two lists or two tuples too.
    let unordered = |kinds: &str| {
        let message = refusal(
            format_args!("`{symbol}` of two {kinds}"),
            "order two ints or two strings",
        );
        let fault = origin(Fault::new(at, message), "the left operand", operands.0);
        origin(fault, "the right operand", operands.1)
    };
    let ordering = match (&operands.0.kind, &operands.1.kind) {
        (Kind::Int(left), Kind::Int(right)) => left.cmp(right),
        (Kind::Str(left), Kind::Str(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Kind::Bool(_), Kind::Bool(_)) => return Err(unordered("bools")),
        (Kind::List(_), Kind::List(_)) => return Err(unordered("lists")),
        (Kind::Tuple(_), Kind::Tuple(_)) => return Err(unordered("tuples")),
        _ => return Err(rejected(symbol, at, operands, "two ints or two strings")),
    };
    Ok(test(ordering))
}

/// Whether the left of `operands` is in the right.
fn contains(symbol: &str, at: Span, operands: (&Value, &Value)) -> Result<bool, Fault> {
    let (needle, haystack) = operands;
    match &haystack.kind {
        Kind::List(items) | Kind::Tuple(items) => Ok(items.iter().any(|item| item.equals(needle))),
        Kind::Dict(dict) => {
            let key = Key::of(needle)
                .map_err(|message| origin(Fault::new(at, message), "the left operand", needle))?;
            Ok(dict.get(&key).is_some())
        }
        Kind::Str(text) => {
            if let Kind::Str(needle) = &needle.kind {
                return Ok(text.contains(&**needle));
            }
            let message = format!(
                "`{symbol}` can't find {} in a string: look for a string in it",
                needle.kind.describe()
            );
            Err(origin(Fault::new(at, message), "the left operand", needle))
        }
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => {
            let message = format!(
                "`{symbol}` can't look in {}: give it a list, a tuple, a dict or a string to look \
                 in",
                haystack.kind.describe()
            );
            Err(origin(
                Fault::new(at, message),
                "the right operand",
                haystack,
            ))
        }
    }
}

fn union(at: Span, operands: (&Value, &Value), budget: &Budget) -> Result<Kind, Fault> {
    match (&operands.0.kind, &operands.1.kind) {
        (Kind::Dict(left), Kind::Dict(right)) => Ok(Kind::Dict(left.union(right, at, budget)?)),
        (Kind::Int(_), Kind::Int(_)) => {
            let message = refusal("`|` of two ints, a bitwise OR,", "write the int it gives");
            Err(Fault::new(at, message))
        }
        _ => Err(rejected("|", at, operands, "two dicts")),
    }
}

fn arithmetic(
    operator: Arithmetic,
    at: Span,
    operands: (&Value, &Value),
    budget: &Budget,
) -> Result<Kind, Fault> {
    let kind = match (operator, &operands.0.kind, &operands.1.kind) {
        (_, Kind::Int(left), Kind::Int(right)) => Kind::Int(int(operator, at, *left, *right)?),
        (Arithmetic::Add, Kind::Str(left), Kind::Str(right)) => {
            Kind::Str(Text::new(&[&**left, &**right].concat(), at, budget)?)
        }
        (Arithmetic::Add, Kind::List(left), Kind::List(right)) => {
            Kind::List(left.concat(right, at, budget)?)
        }
        (Arithmetic::Add, Kind::Tuple(left), Kind::Tuple(right)) => {
            Kind::Tuple(left.concat(right, at, budget)?)
        }
        (Arithmetic::Multiply, Kind::Str(_) | Kind::List(_) | Kind::Tuple(_), Kind::Int(_))
        | (Arithmetic::Multiply, Kind::Int(_), Kind::Str(_) | Kind::List(_) | Kind::Tuple(_)) => {
            let message = refusal(
                "`*` of a sequence and an int, which would let an expression make a value of any \
                 size,",
                "write the elements out, or join with `+`",
            );
            return Err(Fault::new(at, message));
        }
        (Arithmetic::Modulo, Kind::Str(_), _) => {
            let message = refusal(
                "`%` of a string, which formats it,",
                "format it with `format`, or join strings with `+`",
            );
            return Err(Fault::new(at, message));
        }
        _ => {
            let takes = match operator {
                Arithmetic::Add => "two ints, two strings, two lists or two tuples",
                Arithmetic::Subtract
                | Arithmetic::Multiply
                | Arithmetic::FloorDivide
                | Arithmetic::Modulo => "two ints",
            };
            return Err(rejected(operator.symbol(), at, operands, takes));
        }
    };
    Ok(kind)
}

/// What `operator`, at `at`, makes of two ints, refusing what overflows 64 bits.
fn int(operator: Arithmetic, at: Span, left: i64, right: i64) -> Result<i64, Fault> {
    let symbol = operator.symbol();
    let result = match operator {
        Arithmetic::Add => left.checked_add(right),
        Arithmetic::Subtract => left.checked_sub(right),
        Arithmetic::Multiply => left.checked_mul(right),
        Arithmetic::FloorDivide | Arithmetic::Modulo if right == 0 => {
            let message = format!("`{left} {symbol} 0` divides by zero: divide by another int");
            return Err(Fault::new(at, message));
        }
        Arithmetic::FloorDivide => left.checked_div(right).map(|quotient| {
            // Rust's division truncates toward zero; the floor is one less when the
            // quotient is negative and not whole.
            let negative = (left < 0) != (right < 0);
            quotient - i64::from(negative && quotient * right != left)
        }),
        // The remainder by -1 is 0, which Rust's `checked_rem` refuses for the least int.
        Arithmetic::Modulo if right == -1 => Some(0),
        Arithmetic::Modulo => left.checked_rem(right).map(|remainder| {
            // Rust's remainder takes the dividend's sign; the floored one takes the divisor's.
            let differs = remainder != 0 && ((remainder < 0) != (right < 0));
            remainder + if differs { right } else { 0 }
        }),
    };
    result.ok_or_else(|| overflow(at, &format!("{left} {symbol} {right}")))
}

pub(super) fn negate(at: Span, operand: &Value) -> Result<Kind, Fault> {
    let Kind::Int(int) = operand.kind else {
        let message = format!("`-` can't take {}: give it an int", operand.kind.describe());
        return Err(origin(Fault::new(at, message), "the operand", operand));
    };
    let negated = int
        .checked_neg()
        .ok_or_else(|| overflow(at, &format!("-({int})")))?;
    Ok(Kind::Int(negated))
}

fn overflow(at: Span, operation: &str) -> Fault {
    let message = refusal(
        format_args!("`{operation}`, which overflows 64 bits,"),
        "keep ints from -2^63 to 2^63 - 1, since ints are 64-bit",
    );
    Fault::new(at, message)
}

pub(super) fn index(object: &Value, key: &Value, at: Span) -> Result<Value, Fault> {
    let message = match &object.kind {
        Kind::List(items) | Kind::Tuple(items) => {
            let Kind::Int(index) = key.kind else {
                let message = format!(
                    "{} can't index {}: index it with an int",
                    key.kind.describe(),
                    object.kind.describe()
                );
                return Err(origin(Fault::new(at, message), "the index", key));
            };
            // From the end when negative, and no length overflows 64 bits.
            let place = if index < 0 {
                usize::try_from(index.unsigned_abs())
                    .ok()
                    .and_then(|back| items.len().checked_sub(back))
            } else {
                usize::try_from(index).ok()
            };
            if let Some(element) = place.and_then(|place| items.get(place)) {
                return Ok(element.clone());
            }
            let (kind, n) = (object.kind.describe(), items.len());
            if n == 0 {
                format!(
                    "`{index}` indexes no element of {kind} that is empty: index one that has some"
                )
            } else {
                format!(
                    "`{index}` indexes no element of {kind} of {n}: index from -{n} to {}",
                    n - 1
                )
            }
        }
        Kind::Dict(dict) => {
            let lookup = Key::of(key)
                .map_err(|message| origin(Fault::new(at, message), "the index", key))?;
            if let Some(entry) = dict.get(&lookup) {
                return Ok(entry.value.clone());
            }
            format!(
                "`{}` is no key of this dict: index it with one of its keys, or give a default \
                 with `get`",
                lookup.literal()
            )
        }
        Kind::Str(_) => refusal(
            "indexing a string, whose elements Starlark's implementations count differently,",
            "split it, or test it with `startswith`",
        ),
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => {
            format!(
                "{} can't be indexed: index a list, a tuple or a dict",
                object.kind.describe()
            )
        }
    };
    Err(origin(Fault::new(at, message), "the indexed value", object))
}

pub(super) fn elements(items: &Value, at: Span) -> Result<Vec<Value>, Fault> {
    let message = match &items.kind {
        Kind::List(items) | Kind::Tuple(items) => return Ok(items.to_vec()),
        Kind::Dict(dict) => return Ok(dict.iter().map(Entry::key_value).collect()),
        Kind::Str(_) => String::from("a string can't be iterated over: split it into a list"),
        Kind::None
        | Kind::Bool(_)
        | Kind::Int(_)
        | Kind::Function(_)
        | Kind::Builtin(_)
        | Kind::Image(_)
        | Kind::Module(_) => {
            format!(
                "{} can't be iterated over: give a list, a tuple or a dict",
                items.kind.describe()
            )
        }
    };
    Err(origin(Fault::new(at, message), "what is iterated", items))
}
