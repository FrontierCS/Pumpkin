//! Starlark's numbers: an int and a float compare, and key a dict, by their exact values (spec.md,
//! "Comparisons"); and what `//`, `%`, `int` and `str` make of a float, with `core` alone, as the
//! crate is `no_std`.

use alloc::format;
use alloc::string::String;
use core::cmp::Ordering;

use super::{Kind, quote};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    pub(crate) fn of(kind: &Kind) -> Option<Self> {
        match kind {
            Kind::Int(int) => Some(Self::Int(*int)),
            Kind::Float(float) => Some(Self::Float(*float)),
            Kind::None
            | Kind::Bool(_)
            | Kind::Str(_)
            | Kind::List(_)
            | Kind::Tuple(_)
            | Kind::Dict(_)
            | Kind::Function(_)
            | Kind::Builtin(_)
            | Kind::Image(_)
            | Kind::Module(_) => None,
        }
    }

    pub(crate) fn kind(self) -> Kind {
        match self {
            Self::Int(int) => Kind::Int(int),
            Self::Float(float) => Kind::Float(float),
        }
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "an int of magnitude 2^53 and up rounds to the nearest float, as the specification has it"
    )]
    pub(crate) fn float(self) -> f64 {
        match self {
            Self::Int(int) => int as f64,
            Self::Float(float) => float,
        }
    }

    pub(crate) fn literal(self) -> String {
        match self {
            Self::Int(int) => format!("{int}"),
            Self::Float(float) => float_text(float),
        }
    }
}

// NaN is greater than every other number and equal to itself, and `-0.0` equals `0.0`, as
// starlark-rust orders floats:
// https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/types/float/float.rs#L211-L219
// starlark-rust rounds an int to a float to compare the two, so that two ints from 2^53 up would
// equal one float and not each other, and a dict's index would have no order; Bazel's
// `compareIntAndDouble` compares them exactly:
// https://github.com/bazelbuild/bazel/blob/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/main/java/net/starlark/java/eval/StarlarkInt.java#L787-L835
impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (Self::Int(left), Self::Int(right)) => left.cmp(&right),
            (Self::Int(int), Self::Float(float)) => against(int, float),
            (Self::Float(float), Self::Int(int)) => against(int, float).reverse(),
            (Self::Float(left), Self::Float(right)) => {
                (left.partial_cmp(&right)).unwrap_or_else(|| left.is_nan().cmp(&right.is_nan()))
            }
        }
    }
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Number {}

fn against(int: i64, float: f64) -> Ordering {
    if float.is_nan() {
        return Ordering::Less;
    }
    let Some((whole, whole_float)) = truncation(float) else {
        return if float > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    };
    let fraction = if float > whole_float {
        Ordering::Less
    } else if float < whole_float {
        Ordering::Greater
    } else {
        Ordering::Equal
    };
    int.cmp(&whole).then(fraction)
}

/// `float` rounded toward zero, if that is an int.
#[expect(
    clippy::cast_possible_truncation,
    reason = "from -2^63 up to, not at, 2^63, both exact floats, the cast truncates exactly"
)]
pub(crate) fn truncated(float: f64) -> Option<i64> {
    const BOUND: f64 = 9_223_372_036_854_775_808.0;
    (-BOUND..BOUND).contains(&float).then_some(float as i64)
}

// `float` rounded toward zero, as an int and as a float, if that is an int.
#[expect(
    clippy::cast_precision_loss,
    reason = "a float's truncation is exact as a float: from 2^52 up a float has no fraction"
)]
fn truncation(float: f64) -> Option<(i64, f64)> {
    let whole = truncated(float)?;
    Some((whole, whole as f64))
}

pub(crate) fn floor(float: f64) -> f64 {
    // Past the ints a float has no fraction, and NaN and the infinities are their own floor.
    let Some((_, whole)) = truncation(float) else {
        return float;
    };
    // `copysign` keeps `floor(-0.0)` at `-0.0`.
    let whole = whole.copysign(float);
    if whole > float { whole - 1.0 } else { whole }
}

/// The remainder of `dividend` by `divisor`, of the divisor's sign, as `core`'s `%` gives it.
pub(crate) fn modulo(dividend: f64, divisor: f64) -> f64 {
    // `core`'s remainder takes the dividend's sign; a zero one is `0.0`, as starlark-rust's is:
    // https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/types/float/float.rs#L221-L240
    let remainder = dividend % divisor;
    if remainder == 0.0 {
        0.0
    } else if (remainder < 0.0) != (divisor < 0.0) {
        remainder + divisor
    } else {
        remainder
    }
}

/// How `str` writes `float`: the shortest decimal that reads back as it, with a fraction or an
/// exponent, or `nan`, `+inf` or `-inf`.
pub(crate) fn float_text(float: f64) -> String {
    // As starlark-rust's `write_compact`, but for the switch to an exponent, which it takes where
    // `log10` gives -6 or less, or 6 or more, and `core` has no `log10`:
    // https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/types/float/float.rs#L165-L191
    let Some(finite) = Finite::new(float) else {
        let text = if float.is_nan() {
            "nan"
        } else if float > 0.0 {
            "+inf"
        } else {
            "-inf"
        };
        return String::from(text);
    };
    let magnitude = float.abs();
    if magnitude >= 1e6 || (magnitude < 1e-5 && magnitude != 0.0) {
        scientific(finite)
    } else if float % 1.0 == 0.0 {
        format!("{float:.1}")
    } else {
        format!("{float}")
    }
}

/// What `float` makes of `text`, as Rust's `f64::from_str` reads it, or why it makes nothing.
// As starlark-rust's `float`:
// https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/types/float/globals.rs#L56-L88
pub(crate) fn float_of(text: &str) -> Result<f64, String> {
    let shown = quote(text);
    match text.parse::<f64>() {
        // `f64::from_str` reads a text too large for a float as an infinity.
        Ok(float) if !float.is_infinite() || text.to_ascii_lowercase().contains("inf") => Ok(float),
        Ok(_) => Err(format!(
            "`float` of `{shown}` is too large for a float: give it at most {}",
            float_text(f64::MAX)
        )),
        Err(_) => Err(format!(
            "`float` can't read `{shown}`: give it a float's or an int's digits, or `inf` or \
             `nan`, after `-` or `+` or neither"
        )),
    }
}

#[derive(Clone, Copy)]
struct Finite(f64);

impl Finite {
    fn new(float: f64) -> Option<Self> {
        float.is_finite().then_some(Self(float))
    }
}

// `core`'s shortest mantissa, as `1.5e7`, with the exponent's sign and two digits at least, as
// starlark-rust's `write_compact_scientific` writes it:
// https://github.com/facebook/starlark-rust/blob/85e33edead0189db7a8312005ae8175a30bf155c/starlark/src/values/types/float/float.rs#L146-L162
fn scientific(float: Finite) -> String {
    let core = format!("{:e}", float.0);
    let Some((mantissa, exponent)) = core.split_once('e') else {
        unreachable!("`{core}` has no `e`, which `{{:e}}` writes before a finite float's exponent")
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        unreachable!("`{core}` has no int after its `e`, as `{{:e}}` writes a finite float's")
    };
    format!("{mantissa}e{exponent:+03}")
}
