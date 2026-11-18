//! What a record's IR and `provider.json` say of a run's roles.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use schema::ir::{self, Gpu, Name};
use serde::Deserialize;
use serde_json::Value;

use super::Place;
use super::progress::{Sandbox, places};
use crate::TokenPrices;
use crate::read_error::{ReadError, read_optional};

/// What the IR and `provider.json` say of a run's roles: what the log's lines refer to by name.
#[derive(Default)]
pub(super) struct Roles {
    pub(super) agent_role: String,
    pub(super) routed: bool,
    pub(super) grants: BTreeMap<(String, String), Grant>,
    pub(super) final_target: Option<String>,
    pub(super) places: BTreeMap<String, Place>,
    pub(super) gpus: BTreeMap<String, Gpu>,
    pub(super) deadline: Option<Duration>,
    pub(super) startups: BTreeMap<Place, Duration>,
    pub(super) prices: BTreeMap<Place, f64>,
    pub(super) token_prices: BTreeMap<String, TokenPrices>,
}

impl Roles {
    pub(super) fn of(ir: Option<&ir::Workflow>, provided: &Value) -> Self {
        let roles = ir.map_or(&[][..], |ir| ir.roles.as_slice());
        let gpus = (roles.iter())
            .filter_map(|role| Some((role.name.to_string(), role.needs.gpu.clone()?)));
        let agent = roles.iter().find_map(|role| match &role.runtime {
            ir::Runtime::Agent(agent) => Some((&role.name, agent)),
            ir::Runtime::Target(_) | ir::Runtime::Runner(_) => None,
        });
        let grants = roles.iter().flat_map(|role| {
            held(&role.runtime)
                .map(|(name, grant)| ((role.name.to_string(), name.to_string()), grant))
        });
        let final_target = ir.map(|ir| ir.final_call.target.to_string());
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
            agent_role: agent.map(|(name, _)| name.to_string()).unwrap_or_default(),
            routed: agent.is_some_and(|(_, agent)| agent.egress.model.is_some()),
            grants: grants.collect(),
            final_target,
            places: places(provided),
            gpus: gpus.collect(),
            deadline: agent.map(|(_, agent)| Duration::from_secs(agent.deadline_seconds.get())),
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

/// Each grant that `runtime` holds, by its name: the agent's grants, and a target's sessions.
fn held(runtime: &ir::Runtime) -> impl Iterator<Item = (&Name, Grant)> {
    let session = |session: &ir::Session| Grant {
        role: Some(session.runner.to_string()),
        ready_for: Some(Duration::from_secs(session.instance_seconds.get())),
    };
    let (agent, sessions) = match runtime {
        ir::Runtime::Agent(agent) => (agent.grants.as_slice(), &[][..]),
        ir::Runtime::Target(ir::TargetRuntime { sessions, .. }) => (&[][..], sessions.as_slice()),
        ir::Runtime::Runner(_) => (&[][..], &[][..]),
    };
    let agent = agent.iter().map(move |grant| {
        let held = match &grant.function {
            ir::Function::Job(job) => Grant {
                role: Some(job.target.to_string()),
                ready_for: None,
            },
            ir::Function::Session(held) => session(held),
            ir::Function::Now(_) => Grant {
                role: None,
                ready_for: None,
            },
        };
        (&grant.name, held)
    });
    agent.chain(
        sessions
            .iter()
            .map(move |named| (&named.name, session(&named.session))),
    )
}

/// The IR in record `dir`'s `ir.json`, or `None` if it isn't written.
pub(crate) fn ir_in(dir: &Path) -> Result<Option<ir::Workflow>, ReadError> {
    let path = dir.join("ir.json");
    let Some(bytes) = read_optional(&path)? else {
        return Ok(None);
    };
    let ir = serde_json::from_slice(&bytes).map_err(|error| {
        ReadError::Form(format!(
            "{} holds an IR that this build doesn't read: {error}",
            path.display()
        ))
    })?;
    Ok(Some(ir))
}
