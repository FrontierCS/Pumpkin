//! The agent's phase during work and its command's ending in a record.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use runtime::{AgentSpan, CallerName, ReadError, Readout, Record};

use super::child::exited;
use super::model::{answered, failed_upstream, model};
use super::{Phase, clock, in_flight, placed, span};

fn phase(
    name: &str,
    agent: &AgentSpan,
    model: &runtime::ModelUse,
    deadline: Option<Duration>,
    work_started: Option<Duration>,
    at: Duration,
) -> Phase {
    let flying = in_flight(model, at);
    let who = if name == "agent" { "the agent" } else { name };
    let on = placed(&agent.sandbox);
    let Some(started) = agent.sandbox.started else {
        return Phase {
            name: "starting".to_owned(),
            since: Duration::ZERO,
            bound: agent.sandbox.startup,
            doing: format!("{who} on {on}") + &flying,
        };
    };
    let requests = crate::counted(model.requests, "model request");
    // What wasn't good: each stated failure, and each status once one isn't a whole 2xx.
    let statuses = answered(model).filter(|_| model.not_2xx > 0);
    let not_good: Vec<String> = failed_upstream(model).into_iter().chain(statuses).collect();
    let not_good = not_good.join("; ");
    let doing = match (agent.model.is_some(), model.last_good) {
        (false, _) => format!("{who} on {on}"),
        (true, Some(last)) if not_good.is_empty() => format!(
            "{requests}, the last answered {} ago",
            span(at.saturating_sub(last))
        ),
        (true, Some(last)) => format!(
            "{requests}, {not_good}; the last good one answered {} ago",
            span(at.saturating_sub(last))
        ),
        (true, None) if model.requests == 0 => "no model request answered yet".to_owned(),
        (true, None) => format!("{requests}, {not_good}; no good one answered yet"),
    };
    Phase {
        name: name.to_owned(),
        since: started,
        bound: work_started
            .zip(deadline)
            .map(|(start, deadline)| start.saturating_add(deadline)),
        doing: doing + &flying,
    }
}

pub(crate) fn members(
    record: &Record,
    readout: &Readout,
) -> Result<Vec<runtime::MemberEnding>, ReadError> {
    if readout.progress.as_ref().is_some_and(|p| {
        p.agents
            .keys()
            .any(|caller| matches!(caller, CallerName::Member(_)))
    }) {
        record.team_members()
    } else {
        Ok(Vec::new())
    }
}

pub(crate) fn caller_lane(caller: CallerName) -> String {
    caller.to_string().replace('-', " ")
}

pub(crate) fn line(caller: CallerName, agent: &AgentSpan, work_end: Option<Duration>) -> String {
    let name = if caller == CallerName::Agent {
        "agent"
    } else {
        &agent.role
    };
    let mut line = format!("{name} on {}", placed(&agent.sandbox));
    if let Some(started) = agent.sandbox.started {
        let _ = write!(line, ", started at {}", clock(started));
    }
    match &agent.end {
        Some((at, runtime::AgentEnd::Exited(status))) => {
            let _ = write!(line, ", {} at {}", exited(*status), clock(*at));
        }
        Some((at, runtime::AgentEnd::Failed(reason))) => {
            let reason = crate::escaped(reason);
            let _ = write!(line, ", failed at {}: {reason}", clock(*at));
        }
        None => {}
    }
    if let Some(work_end) = work_end.filter(|_| caller == CallerName::Agent) {
        let _ = write!(line, "; the work phase ended at {}", clock(work_end));
    }
    line
}

pub(super) fn work(progress: &runtime::Progress, at: Duration) -> Phase {
    let agents = &progress.agents;
    let flying = in_flight(&progress.model, at);
    if let Some(agent) = agents.get(&CallerName::Agent) {
        return phase(
            "agent",
            agent,
            &progress.model,
            progress.deadline,
            agent.sandbox.started,
            at,
        );
    }
    let working = agents
        .values()
        .filter(|agent| agent.sandbox.gone.is_none())
        .count();
    match progress.work_started {
        Some(started) => Phase {
            name: "team".to_owned(),
            since: started,
            bound: progress
                .deadline
                .map(|deadline| started.saturating_add(deadline)),
            doing: format!("{working} of {} agents working{flying}", agents.len()),
        },
        None => Phase {
            name: "starting".to_owned(),
            since: Duration::ZERO,
            bound: agents
                .values()
                .filter_map(|agent| agent.sandbox.startup)
                .max(),
            doing: crate::counted(agents.len(), "agent") + &flying,
        },
    }
}

pub(super) fn children(
    progress: &runtime::Progress,
    at: Duration,
) -> impl Iterator<Item = ((Duration, Duration), Phase)> {
    let no_route = runtime::ModelUse::default();
    progress
        .agents
        .iter()
        .filter(|(caller, agent)| {
            matches!(caller, CallerName::Member(_)) && agent.sandbox.gone.is_none()
        })
        .map(move |(_, agent)| {
            let child = phase(
                &agent.role,
                agent,
                agent.model.as_ref().unwrap_or(&no_route),
                progress.deadline,
                progress.work_started,
                at,
            );
            ((agent.sandbox.asked, agent.sandbox.asked), child)
        })
}

pub(super) fn summarize(
    summary: &mut String,
    progress: &runtime::Progress,
    members: &[runtime::MemberEnding],
    prices: Option<&BTreeMap<String, runtime::TokenPrices>>,
) {
    for (&caller, agent) in &progress.agents {
        let line = line(caller, agent, progress.work_end);
        let _ = writeln!(summary, "  {:>7} {line}", clock(agent.sandbox.asked));
        if let Some(member) = members
            .iter()
            .find(|member| member.role.as_str() == agent.role)
        {
            let selected = if member.has_selection() {
                "selected"
            } else {
                "no selection"
            };
            let _ = writeln!(summary, "{:>9}   {selected}, {}", "", member.reason());
            if let Some(reason) = &member.route_failure {
                let _ = writeln!(
                    summary,
                    "{:>9}   model route failed: {}",
                    "",
                    crate::escaped(reason)
                );
            }
        }
        if matches!(caller, CallerName::Member(_))
            && let Some(usage) = &agent.model
        {
            let _ = writeln!(summary, "{:>9}   model: {}", "", model(usage, prices));
        }
    }
}
