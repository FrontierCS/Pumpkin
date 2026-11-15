use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::progress::{ModelUse, Place, Progress, Sandbox, Tokens};
use super::{logged, nanos};
use crate::TokenPrices;

impl Progress {
    /// Each sandbox of the run: the agent's, each judge call's, and each lease's.
    fn each_sandbox(&self) -> impl Iterator<Item = &Sandbox> {
        let calls = self.calls.iter().map(|call| &call.sandbox);
        let leases = self.leases.iter().map(|lease| &lease.sandbox);
        std::iter::once(&self.agent.sandbox)
            .chain(calls)
            .chain(leases)
    }

    /// Each place the run's sandboxes were in, with how many, how long they lasted in all, each
    /// from the core's start of it until the run was done with it, or the run's end, or else the
    /// log's last record, how long its leases' runners were charged for ([`LeaseSpan::charged`]),
    /// and busy ([`LeaseSpan::busy`]),
    /// and how many the record says were created, and deleted.
    ///
    /// [`LeaseSpan::charged`]: super::LeaseSpan::charged
    /// [`LeaseSpan::busy`]: super::LeaseSpan::busy
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

    /// What the run's sandboxes cost, in dollars, at their classes' prices, `None` if none
    /// lasted in a class with one, and whether some lasted in a class without one, which the sum
    /// leaves out.
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

    /// What the run cost so far, at the prices that its record keeps; `None` if nothing of it
    /// has a price: no model whose tokens have a cost, no sandbox in a class with a price.
    pub fn cost(&self) -> Option<Cost> {
        let (tokens, unpriced_models) = self.model.usd(&self.token_prices);
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
        })
    }
}

impl ModelUse {
    /// What the tokens of these requests cost, in dollars, at `prices`, each model's by the
    /// route's name for it, `None` if no model's tokens have a cost at them
    /// ([`TokenPrices::usd`]), and the models whose tokens have none, which the sum leaves out.
    pub fn usd(&self, prices: &BTreeMap<String, TokenPrices>) -> (Option<f64>, Vec<String>) {
        let mut usd = None;
        let mut unpriced = Vec::new();
        for (model, tokens) in &self.by_model {
            match prices.get(model).and_then(|prices| prices.usd(tokens)) {
                Some(cost) => *usd.get_or_insert(0.0) += cost,
                None => unpriced.push(model.clone()),
            }
        }
        (usd, unpriced)
    }
}

/// What a run cost, in dollars, at the prices that its record keeps.
///
/// Each sum leaves out what has no price: the models whose tokens have no cost at them, and the
/// sandboxes that lasted in a class without a price, if some did.
#[derive(Debug)]
pub struct Cost {
    pub priced: Priced,
    pub unpriced_models: Vec<String>,
    pub unpriced_classes: bool,
}

/// The parts of a run's cost that have a price, in dollars: its tokens, its sandboxes, or both.
#[derive(Debug)]
pub enum Priced {
    Both { tokens: f64, sandboxes: f64 },
    Tokens(f64),
    Sandboxes(f64),
}

impl Cost {
    /// The whole of what has a price.
    pub fn usd(&self) -> f64 {
        match self.priced {
            Priced::Both { tokens, sandboxes } => tokens + sandboxes,
            Priced::Tokens(usd) | Priced::Sandboxes(usd) => usd,
        }
    }

    /// Whether the run cost more than [`Cost::usd`], by what has no price.
    pub fn unpriced(&self) -> bool {
        !self.unpriced_models.is_empty() || self.unpriced_classes
    }
}

/// The sandboxes a run had in a place.
///
/// How many, how long they lasted in all, how many the record says were created, and deleted,
/// and the price of the place's class, in dollars an hour, if its provider declared one.
#[derive(Debug)]
pub struct PlaceUse {
    pub count: u64,
    pub lasted: Duration,
    /// How long the runners of the leases among them were charged for, if there are any, and how
    /// much of that their commands and transfers ran ([`LeaseSpan::busy`]).
    ///
    /// [`LeaseSpan::busy`]: super::LeaseSpan::busy
    pub charged: Option<(Duration, Duration)>,
    pub created: u64,
    pub deleted: u64,
    pub usd_per_hour: Option<f64>,
}

impl PlaceUse {
    /// What they cost, in dollars, at the price, if there is one.
    pub fn usd(&self) -> Option<f64> {
        Some(self.usd_per_hour? * self.lasted.as_secs_f64() / 3600.0)
    }
}

/// What a run used, as its `outcome.json` records it: `{"ran_ns": r, "model_requests": m, "tokens":
/// t, "calls": [c, ...], "sandboxes": [s, ...], "usd": d, "unpriced_models": [u, ...]}`: when its
/// log last wrote, the model requests its routes answered and the tokens they reported,
/// `{"responses": n, "input": i, "output": o}` with `"cached"`, `"cache_write"` and `"reasoning"`
/// where given; each judge call's share, if it made model requests, `{"job": j, "grant": g,
/// "model_requests": m, "tokens": t}`; each place's sandboxes, `{"provider": p, "class": c,
/// "count": n, "lasted_ns": l}`, with `"charged_ns"` and `"busy_ns"` for leases'
/// ([`PlaceUse::charged`]), `"undeleted"`, and `"usd_per_hour"` and `"usd"` for a priced class;
/// and the run's cost ([`Progress::cost`]) and its unpriced models, if something has a price.
pub(crate) fn usage_of(progress: &Progress) -> Value {
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
            json!({
                "job": call.job,
                "grant": call.grant,
                "model_requests": call.model.requests,
                "tokens": tokens_json(&call.model.tokens),
            })
        })
        .collect();
    if !calls.is_empty() {
        usage["calls"] = json!(calls);
    }
    if let Some(cost) = progress.cost() {
        usage["usd"] = json!(cost.usd());
        if !cost.unpriced_models.is_empty() {
            usage["unpriced_models"] = json!(cost.unpriced_models);
        }
    }
    usage
}

/// What an ended run used, as its `outcome.json` records it.
#[derive(Debug)]
pub struct Used {
    /// When its log last wrote, since the run started.
    pub ran: Duration,
    pub model_requests: u64,
    pub tokens: Tokens,
    /// What it cost, its tokens and its sandboxes, in dollars, if something of it had a price
    /// ([`Progress::cost`]), and whether it cost more, by what has no price, which the sum leaves
    /// out.
    pub usd: Option<f64>,
    pub unpriced: bool,
}

/// What `usage`, an `outcome.json`'s `usage` as [`usage_of`] writes it, says.
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
        unpriced: !unpriced_models.is_empty()
            || sandboxes.iter().any(|placed| placed.usd_per_hour.is_none()),
    })
}

/// The JSON form of `tokens`, as [`usage_of`] gives it.
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
