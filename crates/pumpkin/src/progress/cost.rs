//! The words for what a run cost, from [`runtime::Progress::cost`], each amount to the cent, and
//! for what each role spent of each budgeted entry's model budget.
//!
//! A cost without a price is unknown, never zero: a run of which nothing has a price shows no
//! cost, and a part of it without one is said to lack one. The tokens that a response didn't
//! report, which its provider bills all the same, are unknown too: a cost that leaves them out
//! says so.

use std::collections::BTreeMap;

use runtime::{Cost, ModelUse, Priced, Progress, TokenPrices, TokensCost};
use schema::ir::{self, ApiRoute, ModelBudget, ModelName, Models};

/// The cost column of a run that cost `usd`, in dollars, and more if the cost is `partial`
/// ([`Cost::partial`]), as `$1.20+`.
pub(crate) fn usd(usd: f64, partial: bool) -> String {
    let more = if partial { "+" } else { "" };
    format!("${usd:.2}{more}")
}

/// The end of a model line: what the tokens of `model` cost at `prices`, `; $1.02 at the models'
/// prices`, then `, but for model-b's tokens, which lack a price`, `, but for 1 response's
/// unreported tokens`, or both, `, but for model-b's tokens, which lack a price, and 1 response's
/// unreported tokens`, for what that leaves out. If no model with a price reported tokens: `;
/// model-b's tokens lack a price`, `; 3 responses of priced models reported no tokens`, or both,
/// `; model-b's tokens lack a price, and 3 responses of priced models reported none`. Nothing if
/// there is nothing to say.
pub(super) fn tokens(model: &ModelUse, prices: &BTreeMap<String, TokenPrices>) -> String {
    let TokensCost {
        usd,
        unpriced,
        unreported,
    } = model.usd(prices);
    let owners = owners(&unpriced);
    let unreported = (unreported > 0).then_some(unreported);
    match (usd, owners, unreported) {
        (Some(usd), None, None) => format!("; ${usd:.2} at the models' prices"),
        (Some(usd), Some(owners), None) => {
            format!(
                "; ${usd:.2} at the models' prices, but for {owners} tokens, which lack a price"
            )
        }
        (Some(usd), None, Some(count)) => {
            let unreported = unreported_tokens(count);
            format!("; ${usd:.2} at the models' prices, but for {unreported}")
        }
        (Some(usd), Some(owners), Some(count)) => {
            let unreported = unreported_tokens(count);
            format!(
                "; ${usd:.2} at the models' prices, but for {owners} tokens, which lack a price, \
                 and {unreported}"
            )
        }
        (None, Some(owners), None) => format!("; {owners} tokens lack a price"),
        (None, Some(owners), Some(count)) => {
            let responses = crate::counted(count, "response");
            format!(
                "; {owners} tokens lack a price, and {responses} of priced models reported none"
            )
        }
        (None, None, Some(count)) => {
            let responses = crate::counted(count, "response");
            format!("; {responses} of priced models reported no tokens")
        }
        (None, None, None) => String::new(),
    }
}

/// The tokens that `count` responses left unreported, `1 response's unreported tokens` or
/// `2 responses' unreported tokens`.
fn unreported_tokens(count: u64) -> String {
    if count == 1 {
        "1 response's unreported tokens".to_owned()
    } else {
        format!("{count} responses' unreported tokens")
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
/// sandboxes`, then what the total leaves out: `, and more without a price`, `, and more for 1
/// response's unreported tokens`, or `, and more without a price, and for 1 response's unreported
/// tokens`; of a run of which only one part has a price, that part, then what it leaves out:
/// `$1.02 for tokens, and more for sandboxes, which lack a price`.
pub(crate) fn line(cost: &Cost) -> String {
    let unpriced_models = !cost.unpriced_models.is_empty();
    // Each amount is its exact sum rounded, so the parts may add up to a cent off the whole.
    let (priced, unpriced) = match cost.priced {
        Priced::Both { tokens, sandboxes } => (
            format!(
                "${:.2}, ${tokens:.2} of it for tokens and ${sandboxes:.2} for sandboxes",
                cost.usd()
            ),
            (unpriced_models || cost.unpriced_classes).then_some("without a price"),
        ),
        Priced::Tokens(usd) => (
            format!("${usd:.2} for tokens"),
            cost.unpriced_classes
                .then_some("for sandboxes, which lack a price"),
        ),
        Priced::Sandboxes(usd) => (
            format!("${usd:.2} for sandboxes"),
            unpriced_models.then_some("for tokens, which lack a price"),
        ),
    };
    priced + &more(unpriced, cost.unreported)
}

/// What a cost leaves out, to follow it: `, and more without a price`, `, and more for 1
/// response's unreported tokens`, or both, `, and more without a price, and for 1 response's
/// unreported tokens`, where `unpriced` says what lacks a price. Nothing if it leaves nothing out.
pub(super) fn more(unpriced: Option<&str>, unreported: u64) -> String {
    let unreported = (unreported > 0).then(|| unreported_tokens(unreported));
    match (unpriced, unreported) {
        (None, None) => String::new(),
        (Some(unpriced), None) => format!(", and more {unpriced}"),
        (None, Some(unreported)) => format!(", and more for {unreported}"),
        (Some(unpriced), Some(unreported)) => {
            format!(", and more {unpriced}, and for {unreported}")
        }
    }
}

/// A line for each entry of a role's model route that `ir`, the run's, gives a model budget, in
/// the IR's order, naming the role and the entry's models, with what the role spent of it at as
/// many places as the budget states: `budget trainer (m-task): $20.04 of $20.00, exhausted`.
/// The spend is `at least` that if a response left its tokens unreported or its tokens can't be
/// costed; the budget is `exhausted` once the spend reaches it ([`ModelBudget`]).
pub(super) fn budgets(progress: &Progress, ir: &ir::Workflow) -> Vec<String> {
    let prices = &progress.token_prices;
    let entries = ir.roles.iter().flat_map(|role| {
        let (agent, egress) = match &role.runtime {
            ir::Runtime::Agent(agent) => (true, Some(&agent.egress)),
            ir::Runtime::Target(target) => (false, Some(&target.egress)),
            ir::Runtime::Runner(_) | ir::Runtime::Shared(_) => (false, None),
        };
        let route = egress.and_then(|egress| egress.model.as_ref());
        let entries = route.map_or(&[][..], ir::ModelRoute::as_slice);
        entries
            .iter()
            .map(move |entry| (agent, role.name.as_str(), entry))
    });
    let lines = entries.filter_map(|(agent, name, entry)| {
        let budget = entry.budget.as_ref()?;
        let spent = if agent {
            let agent = progress.agents.values().find(|agent| agent.role == name)?;
            Spent::of(agent.model.as_ref()?, entry, prices)
        } else {
            let own = (progress.calls.iter()).filter(|call| call.role.as_deref() == Some(name));
            own.fold(Spent::default(), |spent, call| {
                spent.plus(Spent::of(&call.model, entry, prices))
            })
        };
        Some(spent.line(name, &entry.models, budget))
    });
    lines.collect()
}

// What a role's requests through an entry spent, as far as their responses reported it.
#[derive(Clone, Copy, Default)]
struct Spent {
    usd: f64,
    // Whether a response left its tokens unreported, or its tokens can't be costed.
    partial: bool,
}

impl Spent {
    fn of(model: &ModelUse, entry: &ApiRoute, prices: &BTreeMap<String, TokenPrices>) -> Self {
        let TokensCost {
            usd,
            unpriced,
            unreported,
        } = model.usd_of(prices, entry);
        Self {
            usd: usd.unwrap_or(0.0),
            partial: unreported > 0 || !unpriced.is_empty(),
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            usd: self.usd + other.usd,
            partial: self.partial || other.partial,
        }
    }

    fn line(&self, role: &str, models: &Models, budget: &ModelBudget) -> String {
        let models: Vec<&str> = models.as_slice().iter().map(ModelName::as_str).collect();
        let floor = if self.partial { "at least " } else { "" };
        let usd = budget.dollars(self.usd);
        let state = if budget.spent_by(self.usd) {
            ", exhausted"
        } else {
            ""
        };
        format!(
            "budget {role} ({}): {floor}{usd} of {budget}{state}",
            models.join(", ")
        )
    }
}
