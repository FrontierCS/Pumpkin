//! The run's estimated cost and each role's spend against its model budget. Missing prices and
//! unreported usage leave a partial estimate when some usage is priced.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use runtime::{Cost, ModelUse, Priced, Progress, TokenPrices, TokensCost};
use schema::ir::{self, ApiRoute, ModelBudget, ModelName, Models};

/// The cost column of a run that cost `usd`, in dollars, and more if the cost is `partial`
/// ([`Cost::partial`]), as `$1.20+`.
pub(crate) fn usd(usd: f64, partial: bool) -> String {
    let more = if partial { "+" } else { "" };
    format!("${usd:.2}{more}")
}

/// A detailed estimate to the nearest millionth of a dollar, with at least two decimal places.
pub(crate) fn dollars(usd: f64) -> String {
    let amount = format!("${usd:.6}");
    let len = amount.trim_end_matches('0').len().max(amount.len() - 4);
    amount[..len].to_owned()
}

pub(super) fn tokens(model: &ModelUse, prices: &BTreeMap<String, TokenPrices>) -> String {
    let cost = model.usd(prices);
    let mut line = String::new();
    if let Some(usd) = cost.usd {
        let _ = write!(line, "; {} at the models' prices", dollars(usd));
    }
    omissions(&mut line, &cost.unpriced, model);
    line
}

fn omissions(line: &mut String, models: &[String], usage: &ModelUse) {
    if !models.is_empty() {
        let _ = write!(line, "; unpriced tokens for {}", models.join(", "));
    }
    let count = usage.unreported.values().sum();
    if count > 0 {
        let _ = write!(line, "; {}", unreported_tokens(count));
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

/// The breakdown and scope of a run's estimate, for its summary and the page's tooltip.
pub(crate) fn line(cost: &Cost, model: &ModelUse) -> String {
    let mut line = match cost.priced {
        Priced::Both { tokens, sandboxes } => format!(
            "{} ({} for tokens, {} for worker sandboxes)",
            dollars(cost.usd()),
            dollars(tokens),
            dollars(sandboxes)
        ),
        Priced::Tokens(usd) => format!("{} for tokens", dollars(usd)),
        Priced::Sandboxes(usd) => format!("{} for worker sandboxes", dollars(usd)),
    };
    omissions(&mut line, &cost.unpriced_models, model);
    if cost.unpriced_classes {
        line.push_str("; some worker sandbox time is unpriced");
    }
    line.push_str(
        "\n    Covers reported model tokens and worker sandbox time at recorded rates. \
         Excludes controller time, image builds, storage, network and provider billing adjustments.",
    );
    line
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
            ir::Runtime::Runner(_) => (false, None),
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
