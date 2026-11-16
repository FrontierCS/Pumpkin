//! What a record's IR and `provider.json` say of a run's roles.

use std::collections::BTreeMap;
use std::time::Duration;

use schema::ir::Gpu;
use serde::Deserialize;
use serde_json::Value;

use super::Place;
use super::progress::{Sandbox, places};
use crate::TokenPrices;

/// What the IR and `provider.json` say of a run's roles: what the log's lines refer to by name.
#[derive(Default)]
pub(super) struct Roles {
    pub(super) agent_role: String,
    pub(super) routed: bool,
    pub(super) grants: BTreeMap<(String, String), Grant>,
    pub(super) score_role: Option<String>,
    pub(super) places: BTreeMap<String, Place>,
    pub(super) gpus: BTreeMap<String, Gpu>,
    pub(super) deadline: Option<Duration>,
    pub(super) startups: BTreeMap<Place, Duration>,
    pub(super) prices: BTreeMap<Place, f64>,
    pub(super) token_prices: BTreeMap<String, TokenPrices>,
}

impl Roles {
    pub(super) fn of(ir: &Value, provided: &Value) -> Self {
        let roles = ir["roles"].as_array().into_iter().flatten();
        let gpus = roles.clone().filter_map(|role| {
            let gpu = Gpu::deserialize(&role["needs"]["gpu"]).ok()?;
            Some((role["name"].as_str()?.to_owned(), gpu))
        });
        let agent = (roles.clone()).find(|role| !role["runtime"]["agent"].is_null());
        let agent = agent.unwrap_or(&Value::Null);
        let agent_role = agent["name"].as_str().unwrap_or_default();
        let held = roles.map(|role| (role["name"].as_str().unwrap_or_default(), &role["grants"]));
        let grants = held.flat_map(|(holder, grants)| {
            let grants = grants.as_array().into_iter().flatten();
            grants.filter_map(move |grant| {
                // `{"judge": g}`, `{"lease": g}` or `{"submit": g}`.
                let (_, grant) = grant.as_object()?.iter().next()?;
                let role = grant["role"].as_str().map(str::to_owned);
                let granted = Grant {
                    role,
                    ready_for: grant["instance_seconds"].as_u64().map(Duration::from_secs),
                };
                Some((
                    (holder.to_owned(), grant["name"].as_str()?.to_owned()),
                    granted,
                ))
            })
        });
        let score = ir["score"]["role"].as_str();
        let (mut startups, mut prices) = (BTreeMap::new(), BTreeMap::new());
        let providers = provided["providers"].as_object().into_iter().flatten();
        for (provider, ready) in providers {
            let classes = ready["classes"].as_object().into_iter().flatten();
            for (class, offered) in classes {
                let place = Place {
                    provider: provider.clone(),
                    class: class.clone(),
                };
                if let Some(price) = offered["usd_per_hour"].as_f64() {
                    prices.insert(place.clone(), price);
                }
                if let Some(seconds) = offered["startup_seconds"].as_u64() {
                    startups.insert(place, Duration::from_secs(seconds));
                }
            }
        }
        // `{"routes": {role: [{"models": {m: {"usd_per_mtok": t}}}]}}`.
        let entries = provided["routes"].as_object().into_iter().flatten();
        let entries = entries.flat_map(|(_, entries)| entries.as_array().into_iter().flatten());
        let models = entries.flat_map(|entry| entry["models"].as_object().into_iter().flatten());
        let token_prices = models.filter_map(|(model, served)| {
            let prices = TokenPrices::deserialize(&served["usd_per_mtok"]).ok()?;
            Some((model.clone(), prices))
        });
        Self {
            agent_role: agent_role.to_owned(),
            routed: !agent["egress"]["model"].is_null(),
            grants: grants.collect(),
            score_role: score.map(str::to_owned),
            places: places(provided),
            gpus: gpus.collect(),
            deadline: (agent["runtime"]["agent"]["deadline_seconds"].as_u64())
                .map(Duration::from_secs),
            startups,
            prices,
            token_prices: token_prices.collect(),
        }
    }

    pub(super) fn sandbox(&self, role: Option<&str>, asked: Duration) -> Sandbox {
        let place = role.and_then(|role| self.places.get(role)).cloned();
        Sandbox {
            startup: (place.as_ref()).and_then(|place| self.startups.get(place).copied()),
            place,
            needs: role.and_then(|role| self.gpus.get(role)).cloned(),
            asked,
            ..Sandbox::default()
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct Grant {
    pub(super) role: Option<String>,
    pub(super) ready_for: Option<Duration>,
}
