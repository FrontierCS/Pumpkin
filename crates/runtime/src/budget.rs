//! Each role's model budget as the run counts it, at the run's configured prices; the budget's
//! own doc ([`ModelBudget`]) says what is counted and what a request past it gets.

use std::collections::BTreeMap;

use route::{Admission, Answer};
use schema::ir::{Model, ModelBudget, ModelName};
use schema::workflow::{RoleId, Workflow};

use crate::price::AllPrices;
use crate::{TokenPrices, Tokens};

/// What [`budgets`] returns.
pub struct Budgets(BTreeMap<RoleId, Limit>);

struct Limit {
    budget: ModelBudget,
    // The models that the budget counts: each model of the role's route.
    counted: BTreeMap<ModelName, AllPrices>,
}

/// The model budget of each role of `workflow` that has one, with what `prices` give each model
/// it counts.
///
/// # Errors
///
/// Fails unless `prices` give every model that a budget counts a price for every kind of token,
/// naming the role, the model and the kinds of token that lack a price.
pub fn budgets(
    workflow: &Workflow,
    prices: &BTreeMap<ModelName, TokenPrices>,
) -> Result<Budgets, String> {
    let mut budgets = BTreeMap::new();
    for (role, sandbox) in workflow.roles() {
        let Some(Model {
            route,
            budget: Some(budget),
        }) = &sandbox.model
        else {
            continue;
        };
        let mut counted = BTreeMap::new();
        for model in (route.as_slice().iter()).flat_map(|entry| entry.models.as_slice()) {
            let kinds = match prices.get(model).map(TokenPrices::all) {
                Some(Ok(all)) => {
                    counted.insert(model.clone(), all);
                    continue;
                }
                Some(Err(kinds)) => kinds,
                None => "`input`, `cached`, `cache_write` or `output`",
            };
            return Err(format!(
                "`{}` has a model budget, but `{model}` has no price for {kinds} tokens: price \
                 them in its `usd_per_mtok` in the configuration, or remove the budget",
                sandbox.name
            ));
        }
        let limit = Limit {
            budget: budget.clone(),
            counted,
        };
        budgets.insert(role, limit);
    }
    Ok(Budgets(budgets))
}

impl Budgets {
    pub(crate) fn unspent(&self) -> BTreeMap<RoleId, Budget<'_>> {
        let budgets = self
            .0
            .iter()
            .map(|(role, limit)| (*role, Budget { limit, usd: 0.0 }));
        budgets.collect()
    }
}

pub(crate) struct Budget<'r> {
    limit: &'r Limit,
    // What the role's responses cost, in US dollars.
    usd: f64,
}

impl<'r> Budget<'r> {
    /// Adds what the tokens that `answer` reported cost.
    pub(crate) fn add(&mut self, answer: &Answer) {
        let (Answer::Forwarded {
            model,
            usage: Some(usage),
            ..
        }
        | Answer::Cut {
            model,
            usage: Some(usage),
            ..
        }) = answer
        else {
            return;
        };
        let Some(prices) = self.limit.counted.get(model) else {
            unreachable!(
                "`{model}` answered a request of a role whose budget counts every model of its \
                 route, and an answer's model is one that the route allows"
            )
        };
        let mut tokens = Tokens::default();
        tokens.add(usage);
        self.usd += prices.usd(&tokens).unwrap_or(0.0);
    }

    pub(crate) fn admission(&self) -> Admission<'r> {
        let budget = &self.limit.budget;
        if budget.spent_by(self.usd) {
            Admission::Spent { budget }
        } else {
            Admission::Open
        }
    }
}
