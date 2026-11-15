//! The words for what a run cost, from [`runtime::Progress::cost`], each amount to the cent.
//!
//! A cost without a price is unknown, never zero: a run of which nothing has a price shows no
//! cost, and a part of it without one is said to lack one.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use runtime::{Cost, ModelUse, Priced, TokenPrices};

/// The cost column of a run that cost `usd`, in dollars, and more if something of it was
/// `unpriced`, as `$1.20+`.
pub(crate) fn usd(usd: f64, unpriced: bool) -> String {
    let more = if unpriced { "+" } else { "" };
    format!("${usd:.2}{more}")
}

/// What the tokens of `model`, a run's or a judge call's requests, cost at `prices`, as a model
/// line of a summary ends it: `; $1.02 at the models' prices`, then `, but for model-b's tokens,
/// which lack a price` if some have no cost, or `; model-b's tokens lack a price` if none has
/// one; nothing if the requests reported no tokens of a model.
pub(super) fn tokens(model: &ModelUse, prices: &BTreeMap<String, TokenPrices>) -> String {
    let (usd, unpriced) = model.usd(prices);
    let owners = owners(&unpriced);
    match (usd, owners) {
        (Some(usd), None) => format!("; ${usd:.2} at the models' prices"),
        (Some(usd), Some(owners)) => {
            format!(
                "; ${usd:.2} at the models' prices, but for {owners} tokens, which lack a price"
            )
        }
        (None, Some(owners)) => format!("; {owners} tokens lack a price"),
        (None, None) => String::new(),
    }
}

/// `models` as owners of tokens, `model-a's, model-b's and model-c's`; `None` if there are none.
fn owners(models: &[String]) -> Option<String> {
    let (last, others) = models.split_last()?;
    let others: Vec<String> = others.iter().map(|model| format!("{model}'s")).collect();
    Some(if others.is_empty() {
        format!("{last}'s")
    } else {
        format!("{} and {last}'s", others.join(", "))
    })
}

/// The cost line of a summary, after `cost: `: `$1.13, $1.02 of it for tokens and $0.11 for
/// sandboxes`, then `, and more without a price` if something has none; of a run of which only
/// one part has a price, that part, then the other if something of it lacks one: `$1.02 for
/// tokens, and more for sandboxes, which lack a price`.
pub(crate) fn line(cost: &Cost) -> String {
    // Each amount is its exact sum rounded, so the parts may add up to a cent off the whole.
    match cost.priced {
        Priced::Both { tokens, sandboxes } => {
            let mut line = format!(
                "${:.2}, ${tokens:.2} of it for tokens and ${sandboxes:.2} for sandboxes",
                cost.usd()
            );
            if cost.unpriced() {
                line.push_str(", and more without a price");
            }
            line
        }
        Priced::Tokens(usd) => alone(usd, "tokens", "sandboxes", cost.unpriced_classes),
        Priced::Sandboxes(usd) => {
            alone(usd, "sandboxes", "tokens", !cost.unpriced_models.is_empty())
        }
    }
}

/// The cost line of a run of which only `part` has a price, `usd`: `$1.02 for tokens`, then
/// `, and more for sandboxes, which lack a price` if some of `other` was used.
fn alone(usd: f64, part: &str, other: &str, other_used: bool) -> String {
    let mut line = format!("${usd:.2} for {part}");
    if other_used {
        let _ = write!(line, ", and more for {other}, which lack a price");
    }
    line
}
