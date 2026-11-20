//! What a run used: its sandboxes' time, its model requests and tokens, and what they cost in US
//! dollars where a price is known, totalled from a [`Progress`]; and the `usage` object of
//! `outcome.json`, which keeps the totals once the run has ended.
//!
//! A cost counts only what has a price, and says what hasn't ([`Cost`]), so a total never passes
//! for more than it covers.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use schema::ir::{Api, ApiRoute};
use schema::workflow::Workflow;
use serde::Deserialize;
use serde_json::{Value, json};

use super::call::CallOf;
use super::progress::{ModelUse, Place, Progress, Sandbox, Tokens};
use super::{logged, nanos};
use crate::TokenPrices;

impl Progress {
    fn each_sandbox(&self) -> impl Iterator<Item = &Sandbox> {
        let calls = self.calls.iter().map(|call| &call.sandbox);
        let leases = self.leases.iter().map(|lease| &lease.sandbox);
        std::iter::once(&self.agent.sandbox)
            .chain(calls)
            .chain(leases)
    }

    /// The run's sandboxes by place, as far as the log says: until the run's end, or the last line
    /// read if it hasn't ended.
    pub fn sandboxes(&self) -> BTreeMap<Place, PlaceUse> {
        let now = self.end.unwrap_or(self.last);
        let mut sandboxes: BTreeMap<Place, PlaceUse> = BTreeMap::new();
        for sandbox in self.each_sandbox() {
            let Some(place) = &sandbox.place else {
                continue;
            };
            let used = sandboxes.entry(place.clone()).or_insert_with(|| PlaceUse {
                count: 0,
                lasted: Duration::ZERO,
                charged: None,
                created: 0,
                deleted: 0,
                usd_per_hour: self.prices.get(place).copied(),
            });
            used.count += 1;
            used.lasted += sandbox.gone.unwrap_or(now).saturating_sub(sandbox.asked);
            used.created += u64::from(sandbox.deleted.is_some());
            used.deleted += u64::from(sandbox.deleted == Some(true));
        }
        for lease in &self.leases {
            let used = (lease.sandbox.place.as_ref()).and_then(|place| sandboxes.get_mut(place));
            if let Some(used) = used {
                let (charged, busy) = used.charged.get_or_insert_default();
                *charged += lease.charged(now);
                *busy += lease.busy(now);
            }
        }
        sandboxes
    }

    fn usd(&self) -> (Option<f64>, bool) {
        let sandboxes = self.sandboxes();
        let mut usd = None;
        for each in sandboxes.values().filter_map(PlaceUse::usd) {
            *usd.get_or_insert(0.0) += each;
        }
        (
            usd,
            sandboxes.values().any(|used| used.usd_per_hour.is_none()),
        )
    }

    /// What the run cost, as far as it is priced; `None` if nothing is.
    pub fn cost(&self) -> Option<Cost> {
        let TokensCost {
            usd: tokens,
            unpriced: unpriced_models,
            unreported,
        } = self.model.usd(&self.token_prices);
        let (sandboxes, unpriced_classes) = self.usd();
        let priced = match (tokens, sandboxes) {
            (Some(tokens), Some(sandboxes)) => Priced::Both { tokens, sandboxes },
            (Some(tokens), None) => Priced::Tokens(tokens),
            (None, Some(sandboxes)) => Priced::Sandboxes(sandboxes),
            (None, None) => return None,
        };
        Some(Cost {
            priced,
            unpriced_models,
            unpriced_classes,
            unreported,
        })
    }
}

impl ModelUse {
    /// What the tokens that responses reported cost at `prices`, in US dollars, and what that
    /// leaves out.
    pub fn usd(&self, prices: &BTreeMap<String, TokenPrices>) -> TokensCost {
        self.cost(prices, |_| true)
    }

    /// As [`usd`](Self::usd), of the responses through `entry` alone: those for its API and its
    /// models.
    pub fn usd_of(&self, prices: &BTreeMap<String, TokenPrices>, entry: &ApiRoute) -> TokensCost {
        self.cost(prices, |(api, model)| {
            *api == entry.api
                && (entry.models.as_slice().iter()).any(|counted| counted.as_str() == model)
        })
    }

    fn cost(
        &self,
        prices: &BTreeMap<String, TokenPrices>,
        counted: impl Fn(&(Api, String)) -> bool,
    ) -> TokensCost {
        let mut cost = TokensCost::default();
        let keys = self.by_model.keys().chain(self.unreported.keys());
        let keys: BTreeSet<&(Api, String)> = keys.filter(|key| counted(key)).collect();
        for key in keys {
            let (_, model) = key;
            let unreported = self.unreported.get(key).copied().unwrap_or(0);
            cost.add(model, prices.get(model), self.by_model.get(key), unreported);
        }
        // A model may have answered through entries of two APIs.
        cost.unpriced.sort_unstable();
        cost.unpriced.dedup();
        cost
    }
}

/// What the tokens that responses reported cost, and what that leaves out.
#[derive(Debug, Default)]
pub struct TokensCost {
    /// What the tokens that responses reported cost, in US dollars; `None` if no model that
    /// reported tokens has a price.
    pub usd: Option<f64>,
    /// The models that have no price, or whose tokens their prices can't cost
    /// ([`TokenPrices::usd`]).
    pub unpriced: Vec<String>,
    /// How many responses of the models that have a price went with tokens unreported
    /// ([`ModelUse::unreported`]), whose cost `usd` leaves out in whole or in part.
    pub unreported: u64,
}

impl TokensCost {
    fn add(
        &mut self,
        model: &str,
        prices: Option<&TokenPrices>,
        tokens: Option<&Tokens>,
        missing: u64,
    ) {
        match (prices, tokens) {
            (Some(_), None) => self.unreported += missing,
            (Some(prices), Some(tokens)) if let Some(usd) = prices.usd(tokens) => {
                *self.usd.get_or_insert(0.0) += usd;
                self.unreported += missing;
            }
            _ => self.unpriced.push(model.to_owned()),
        }
    }
}

/// A run's cost in US dollars, and what the total leaves out.
#[derive(Debug)]
pub struct Cost {
    /// What the total covers.
    pub priced: Priced,
    /// The models whose tokens went unpriced.
    pub unpriced_models: Vec<String>,
    /// Whether a sandbox's class has no price, so its time went unpriced.
    pub unpriced_classes: bool,
    /// How many responses of the models that have a price went with tokens unreported
    /// ([`ModelUse::unreported`]).
    pub unreported: u64,
}

/// Which of a run's costs have a price, and what each comes to in US dollars.
#[derive(Debug)]
pub enum Priced {
    Both { tokens: f64, sandboxes: f64 },
    Tokens(f64),
    Sandboxes(f64),
}

impl Cost {
    /// The total of what is priced.
    pub fn usd(&self) -> f64 {
        match self.priced {
            Priced::Both { tokens, sandboxes } => tokens + sandboxes,
            Priced::Tokens(usd) | Priced::Sandboxes(usd) => usd,
        }
    }

    /// Whether the total leaves something out: it is then a lower bound.
    pub fn partial(&self) -> bool {
        !self.unpriced_models.is_empty() || self.unpriced_classes || self.unreported > 0
    }
}

/// The sandboxes of one place.
#[derive(Debug)]
pub struct PlaceUse {
    /// How many of the run.
    pub count: u64,
    /// The time from each one's being asked for to its end, or to now, summed.
    pub lasted: Duration,
    /// For the leases among them, how long their runners were ready, and how much of that a
    /// command or a transfer ran, each summed (`LeaseSpan::charged` and `LeaseSpan::busy`);
    /// `None` if none is a lease's.
    pub charged: Option<(Duration, Duration)>,
    /// How many have a `sandbox.json`, so were created.
    pub created: u64,
    /// How many of those that `sandbox.json` records as deleted.
    pub deleted: u64,
    /// The class's price an hour, if it has one.
    pub usd_per_hour: Option<f64>,
}

impl PlaceUse {
    /// What `lasted` costs at the price; `None` if the class has none.
    pub fn usd(&self) -> Option<f64> {
        Some(self.usd_per_hour? * self.lasted.as_secs_f64() / 3600.0)
    }
}

/// The `usage` of `outcome.json`, which [`used`] reads back: `ran_ns`, when the log last wrote;
/// `model_requests`; `tokens`, with `responses`, `input`, `output` and, if some response reported
/// them, `cached`, `cache_write` and `reasoning`; `sandboxes`, one object a place, with `provider`,
/// `class`, `count`, `lasted_ns`, `charged_ns` and `busy_ns` if a lease ran there, `undeleted` if
/// some weren't deleted, and `usd_per_hour` and `usd` if its class is priced; `calls`, if a call
/// made model requests, each with its `job`, `grant`, `model_requests` and `tokens`; `routes` for
/// the used entries of each role, as `docs/record.md` describes; and `usd`
/// with `unpriced_models` and `unreported_responses` ([`Cost`]), unless nothing is priced.
pub(crate) fn usage_of(progress: &Progress, workflow: &Workflow) -> Value {
    let sandboxes = progress.sandboxes().into_iter().map(|(place, used)| {
        let mut sandboxes = json!({
            "provider": place.provider,
            "class": place.class,
            "count": used.count,
            "lasted_ns": nanos(used.lasted),
        });
        if let Some((charged, busy)) = used.charged {
            sandboxes["charged_ns"] = json!(nanos(charged));
            sandboxes["busy_ns"] = json!(nanos(busy));
        }
        if used.deleted < used.created {
            sandboxes["undeleted"] = json!(used.created - used.deleted);
        }
        if let (Some(price), Some(usd)) = (used.usd_per_hour, used.usd()) {
            sandboxes["usd_per_hour"] = json!(price);
            sandboxes["usd"] = json!(usd);
        }
        sandboxes
    });
    let mut usage = json!({
        "ran_ns": nanos(progress.last),
        "model_requests": progress.model.requests,
        "tokens": tokens_json(&progress.model.tokens),
        "sandboxes": sandboxes.collect::<Vec<Value>>(),
    });
    let calls = progress.calls.iter().filter(|call| call.model.requests > 0);
    let calls: Vec<Value> = calls
        .map(|call| {
            let grant = match &call.of {
                CallOf::Function(function) => Some(function),
                CallOf::Final => None,
            };
            json!({
                "job": call.job,
                "grant": grant,
                "model_requests": call.model.requests,
                "tokens": tokens_json(&call.model.tokens),
            })
        })
        .collect();
    if !calls.is_empty() {
        usage["calls"] = Value::Array(calls);
    }
    let routes = routes(progress, workflow);
    if !routes.is_empty() {
        usage["routes"] = Value::Array(routes);
    }
    if let Some(cost) = progress.cost() {
        usage["usd"] = json!(cost.usd());
        if !cost.unpriced_models.is_empty() {
            usage["unpriced_models"] = json!(cost.unpriced_models);
        }
        if cost.unreported > 0 {
            usage["unreported_responses"] = json!(cost.unreported);
        }
    }
    usage
}

fn routes(progress: &Progress, workflow: &Workflow) -> Vec<Value> {
    if progress.route_requests.is_empty() {
        return Vec::new();
    }
    let mut callers: BTreeMap<&str, Vec<&ModelUse>> = BTreeMap::new();
    if let Some(agent) = &progress.agent.model {
        let name = &workflow.sandbox(workflow.agent().role).name;
        callers.entry(name.as_str()).or_default().push(agent);
    }
    for call in &progress.calls {
        if let Some(role) = &call.role {
            callers.entry(role).or_default().push(&call.model);
        }
    }
    let mut routes = Vec::new();
    for (role, sandbox) in workflow.roles() {
        let name = sandbox.name.as_str();
        let Some(entries) = progress.route_requests.get(name) else {
            continue;
        };
        let Some((route, callers)) = workflow.model(role).zip(callers.get(name)) else {
            continue;
        };
        for (index, entry) in route.as_slice().iter().enumerate() {
            if let Some(requests) = entries.get(&index) {
                let mut row = route_usage(entry, callers, &progress.token_prices);
                row["role"] = json!(name);
                row["entry"] = json!(index);
                row["model_requests"] = json!(requests);
                routes.push(row);
            }
        }
    }
    routes
}

fn route_usage(
    entry: &ApiRoute,
    callers: &[&ModelUse],
    prices: &BTreeMap<String, TokenPrices>,
) -> Value {
    let mut tokens = Tokens::default();
    let mut cost = TokensCost::default();
    let mut unreported = 0;
    for model in entry.models.as_slice() {
        let key = (entry.api, model.to_string());
        let mut reported = None::<Tokens>;
        let mut missing = 0;
        for caller in callers {
            if let Some(used) = caller.by_model.get(&key) {
                reported.get_or_insert_default().plus(used);
            }
            missing += caller.unreported.get(&key).copied().unwrap_or(0);
        }
        if let Some(reported) = &reported {
            tokens.plus(reported);
        } else if missing == 0 {
            continue;
        }
        cost.add(
            model.as_str(),
            prices.get(model.as_str()),
            reported.as_ref(),
            missing,
        );
        unreported += missing;
    }
    let mut row = json!({ "api": entry.api, "tokens": tokens_json(&tokens) });
    if unreported == 0 && tokens.reported == 0 {
        row["usd"] = json!(0.0);
    } else if let Some(usd) = cost.usd {
        row["usd"] = json!(usd);
    }
    if !cost.unpriced.is_empty() {
        row["unpriced_models"] = json!(cost.unpriced);
    }
    if unreported > 0 {
        row["unreported_responses"] = json!(unreported);
    }
    row
}

/// What a finished run used, as the `usage` of its `outcome.json` says.
#[derive(Debug)]
pub struct Used {
    /// When its log last wrote, since the run started.
    pub ran: Duration,
    pub model_requests: u64,
    pub tokens: Tokens,
    /// What it cost, its tokens and its sandboxes, in dollars; `None` if nothing is priced.
    pub usd: Option<f64>,
    /// Whether `usd` leaves something out, as [`Cost::partial`] says it of a total.
    pub partial: bool,
}

/// The [`Used`] that a `usage` says, or why it isn't one.
pub(crate) fn used(usage: &Value) -> Result<Used, String> {
    #[derive(Deserialize)]
    struct Usage {
        #[serde(deserialize_with = "logged::nanos")]
        ran_ns: Duration,
        model_requests: u64,
        tokens: Counted,
        sandboxes: Vec<Placed>,
        usd: Option<f64>,
        #[serde(default)]
        unpriced_models: Vec<String>,
        #[serde(default)]
        unreported_responses: u64,
    }
    #[derive(Deserialize)]
    struct Counted {
        responses: u64,
        input: u64,
        output: u64,
        cached: Option<u64>,
        cache_write: Option<u64>,
        reasoning: Option<u64>,
    }
    #[derive(Deserialize)]
    struct Placed {
        usd_per_hour: Option<f64>,
    }
    let Usage {
        ran_ns,
        model_requests,
        tokens,
        sandboxes,
        usd,
        unpriced_models,
        unreported_responses,
    } = Usage::deserialize(usage).map_err(|error| error.to_string())?;
    let Counted {
        responses,
        input,
        output,
        cached,
        cache_write,
        reasoning,
    } = tokens;
    Ok(Used {
        ran: ran_ns,
        model_requests,
        tokens: Tokens {
            reported: responses,
            input,
            output,
            cached,
            cache_write,
            reasoning,
        },
        usd,
        partial: !unpriced_models.is_empty()
            || unreported_responses > 0
            || sandboxes.iter().any(|placed| placed.usd_per_hour.is_none()),
    })
}

fn tokens_json(tokens: &Tokens) -> Value {
    let Tokens {
        reported,
        input,
        output,
        cached,
        cache_write,
        reasoning,
    } = tokens;
    let mut json = json!({ "responses": reported, "input": input, "output": output });
    if let Some(cached) = cached {
        json["cached"] = json!(cached);
    }
    if let Some(written) = cache_write {
        json["cache_write"] = json!(written);
    }
    if let Some(reasoning) = reasoning {
        json["reasoning"] = json!(reasoning);
    }
    json
}
