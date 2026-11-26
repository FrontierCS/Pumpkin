//! What a record's IR and `provider.json` say of a run's roles.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use schema::ir::{self, CompanionName, Gpu, Name};
use serde::Deserialize;
use serde_json::Value;

use super::progress::{Sandbox, places};
use super::{CallerName, Place};
use crate::TokenPrices;
use crate::read_error::{ReadError, read_optional};

/// What the IR and `provider.json` say of a run's roles: what the log's lines refer to by name.
#[derive(Default)]
pub(super) struct Roles {
    pub(super) agents: BTreeMap<CallerName, Agent>,
    pub(super) shared: BTreeMap<String, usize>,
    pub(super) grants: BTreeMap<(String, String), Grant>,
    pub(super) final_target: Option<String>,
    pub(super) places: BTreeMap<String, Place>,
    pub(super) gpus: BTreeMap<String, Gpu>,
    pub(super) companions: BTreeMap<String, Vec<CompanionName>>,
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
        let companions = roles.iter().filter_map(|role| {
            let ir::Runtime::Runner(runner) = &role.runtime else {
                return None;
            };
            let names = runner
                .companions
                .iter()
                .map(|companion| companion.name.clone());
            Some((role.name.to_string(), names.collect()))
        });
        let agents = roles
            .iter()
            .enumerate()
            .filter_map(|(index, role)| {
                let ir::Runtime::Agent(agent) = &role.runtime else {
                    return None;
                };
                Some((
                    CallerName::Agent(index),
                    Agent {
                        name: role.name.to_string(),
                        routed: agent.egress.model.is_some(),
                    },
                ))
            })
            .collect();
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
        Self {
            agents,
            shared: roles
                .iter()
                .enumerate()
                .filter(|(_, role)| matches!(&role.runtime, ir::Runtime::Target(target) if target.lifetime == Some(ir::TargetLifetime::WorkPhase)))
                .map(|(index, role)| (role.name.to_string(), index))
                .collect(),
            grants: grants.collect(),
            final_target,
            places: places(provided),
            gpus: gpus.collect(),
            companions: companions.collect(),
            deadline: ir.map(|ir| Duration::from_secs(ir.deadline_seconds.get())),
            startups,
            prices,
            token_prices: token_prices(provided),
        }
    }

    pub(super) fn agent_name(&self, caller: CallerName) -> Option<&str> {
        self.agents.get(&caller).map(|role| role.name.as_str())
    }

    pub(super) fn sandbox(&self, role: Option<&str>, asked: Duration) -> Sandbox {
        let place = role.and_then(|role| self.places.get(role)).cloned();
        Sandbox {
            startup: (place.as_ref()).and_then(|place| self.startups.get(place).copied()),
            place,
            needs: role.and_then(|role| self.gpus.get(role)).cloned(),
            companions: role.and_then(|role| self.companions.get(role)).cloned(),
            asked,
            ..Sandbox::default()
        }
    }
}

fn token_prices(provided: &Value) -> BTreeMap<String, TokenPrices> {
    // `{"routes": {role: [{"models": {m: {"usd_per_mtok": t}}}]}}`.
    let entries = provided["routes"].as_object().into_iter().flatten();
    let entries = entries.flat_map(|(_, entries)| entries.as_array().into_iter().flatten());
    let models = entries.flat_map(|entry| entry["models"].as_object().into_iter().flatten());
    models
        .filter_map(|(model, served)| {
            let prices = TokenPrices::deserialize(&served["usd_per_mtok"]).ok()?;
            Some((model.clone(), prices))
        })
        .collect()
}

#[derive(Default)]
pub(super) struct Agent {
    pub(super) name: String,
    pub(super) routed: bool,
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
    let ir = serde_json::from_slice(&bytes)
        .or_else(|error| legacy_ir(&bytes).ok_or(error))
        .map_err(|error| {
            ReadError::Form(format!(
                "{} holds an IR that this build doesn't read: {error}",
                path.display()
            ))
        })?;
    Ok(Some(ir))
}

/// `bytes` read as an IR in an older form, which a record may hold. A role's `shared` runtime
/// reads as a `work_phase` target. A target's `lifetime` of `call` or `work` reads as `per_call`
/// or `work_phase`. `None` if no role holds an older form, or the result isn't an IR. Only record
/// readers accept it.
fn legacy_ir(bytes: &[u8]) -> Option<ir::Workflow> {
    let mut ir: Value = serde_json::from_slice(bytes).ok()?;
    let mut converted = false;
    for role in ir.get_mut("roles")?.as_array_mut()? {
        let runtime = role.get_mut("runtime")?.as_object_mut()?;
        if runtime.len() == 1
            && let Some(mut shared) = runtime.remove("shared")
        {
            let target = shared.as_object_mut()?;
            ir::Command::deserialize(target.remove("command")?).ok()?;
            target.insert("lifetime".to_owned(), Value::from("work_phase"));
            runtime.insert("target".to_owned(), shared);
            converted = true;
        }
        let lifetime = runtime
            .get_mut("target")
            .and_then(|target| target.get_mut("lifetime"));
        if let Some(lifetime) = lifetime {
            *lifetime = Value::from(match lifetime.as_str() {
                Some("call") => "per_call",
                Some("work") => "work_phase",
                _ => continue,
            });
            converted = true;
        }
    }
    converted.then_some(())?;
    serde_json::from_value(ir).ok()
}
