//! The model budget of each entry of a role's route as the run counts it, at the run's configured
//! prices; the budget's own doc ([`ModelBudget`]) says what is counted and what a request past it
//! gets.

use std::collections::BTreeMap;

use route::{Admission, Answer};
use schema::ir::{Api, ApiRoute, ModelBudget, ModelName};
use schema::workflow::{RoleId, Workflow};

use crate::price::AllPrices;
use crate::{TokenPrices, Tokens};

/// What [`budgets`] returns.
pub struct Budgets(BTreeMap<RoleId, Vec<Limit>>);

struct Limit {
    // The entry's index in the route.
    entry: usize,
    api: Api,
    budget: ModelBudget,
    // Each model that the entry allows.
    counted: BTreeMap<ModelName, AllPrices>,
}

/// The model budget of each entry of each route of `workflow` that has one, with what `prices`
/// give each model of the entry.
///
/// # Errors
///
/// Fails unless `prices` give every model of an entry with a budget a price for every kind of
/// token, naming the role, the model and the kinds of token that lack a price.
pub fn budgets(
    workflow: &Workflow,
    prices: &BTreeMap<ModelName, TokenPrices>,
) -> Result<Budgets, String> {
    let mut budgets = BTreeMap::new();
    for (role, sandbox) in workflow.roles() {
        let Some(route) = &sandbox.model else {
            continue;
        };
        let mut limits = Vec::new();
        for (entry, route) in route.as_slice().iter().enumerate() {
            let ApiRoute {
                api,
                models,
                request_headers: _,
                response_headers: _,
                budget: Some(budget),
            } = route
            else {
                continue;
            };
            let mut counted = BTreeMap::new();
            for model in models.as_slice() {
                let kinds = match prices.get(model).map(TokenPrices::all) {
                    Some(Ok(all)) => {
                        counted.insert(model.clone(), all);
                        continue;
                    }
                    Some(Err(kinds)) => kinds,
                    None => "`input`, `cached`, `cache_write` or `output`",
                };
                return Err(format!(
                    "`{}` has a model budget, but `{model}` has no price for {kinds} tokens: \
                     price them in its `usd_per_mtok` in the configuration, or remove the budget",
                    sandbox.name
                ));
            }
            limits.push(Limit {
                entry,
                api: *api,
                budget: budget.clone(),
                counted,
            });
        }
        if !limits.is_empty() {
            budgets.insert(role, limits);
        }
    }
    Ok(Budgets(budgets))
}

impl Budgets {
    pub(crate) fn unspent(&self) -> BTreeMap<RoleId, Budget<'_>> {
        let budgets = self.0.iter().map(|(role, limits)| {
            let spends = limits.iter().map(|limit| Spend { limit, usd: 0.0 });
            (*role, Budget(spends.collect()))
        });
        budgets.collect()
    }
}

/// What a role has spent of the budget of each entry of its route that has one.
pub(crate) struct Budget<'r>(Vec<Spend<'r>>);

struct Spend<'r> {
    limit: &'r Limit,
    // What the responses through the entry cost, in US dollars.
    usd: f64,
}

impl<'r> Budget<'r> {
    /// Adds what the tokens that `answer`, through an entry for `api`, reported cost to the
    /// budget of the entry, if it has one.
    pub(crate) fn add(&mut self, api: Api, answer: &Answer) {
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
        // A model is in one entry alone for its API, so the API and the model name the entry.
        let entry = (self.0.iter_mut()).find_map(|Spend { limit, usd }| {
            let prices = (limit.api == api)
                .then(|| limit.counted.get(model))
                .flatten()?;
            Some((prices, usd))
        });
        let Some((prices, usd)) = entry else {
            return;
        };
        let mut tokens = Tokens::default();
        tokens.add(usage);
        *usd += prices.usd(&tokens).unwrap_or(0.0);
    }

    /// The admission of the role's requests through each entry of its route, as spent so far.
    pub(crate) fn spent(&self) -> Spent<'r> {
        let spent = self
            .0
            .iter()
            .filter(|spend| spend.limit.budget.spent_by(spend.usd));
        Spent(
            spent
                .map(|spend| (spend.limit.entry, &spend.limit.budget))
                .collect(),
        )
    }
}

/// The entries of a role's route whose budgets are spent, each by its index, with its budget.
pub(crate) struct Spent<'r>(Vec<(usize, &'r ModelBudget)>);

impl<'r> Spent<'r> {
    /// The admission of a request through the entry at index `entry`.
    pub(crate) fn admission(&self, entry: usize) -> Admission<'r> {
        match self.0.iter().find(|(spent, _)| *spent == entry) {
            Some((_, budget)) => Admission::Spent { budget },
            None => Admission::Open,
        }
    }
}
