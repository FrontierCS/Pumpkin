//! What `cargo xtask live` finds wrong in a run's record, from its parsed lines and values alone,
//! so that its tests reach no provider.

use serde_json::Value;

/// The phases that the Modal helper times of each create, in order.
const PHASES: [&str; 4] = ["lookup", "create", "tunnels", "list"];

/// The phase that the Modal helper times after [`PHASES`] for a sandbox that holds assets.
const ASSETS: &str = "assets";

/// The phase that the Modal helper times last for a sandbox that holds companions.
const COMPANIONS: &str = "companions";

/// The longest that the median exchange between two containers of a sandbox may take, in
/// milliseconds.
// Modal's bridge carries such an exchange in about a millisecond, and a link through the gateway
// in hundreds. So only a path inside the sandbox passes.
const BRIDGE_MEDIAN_MS: f64 = 50.0;

/// What is wrong with each create on Modal in `lines`, a run's log, as [`create_unphased`] says, or
/// a log without one. A sandbox holds companions if the log says its containers met, and assets if
/// `assets`. A Modal sandbox's id starts with `sb-`, and its delete in the log names it.
pub(crate) fn unphased(lines: &[Value], assets: bool) -> Vec<String> {
    let logged = |kind: &str, sandbox: &Value| {
        lines.iter().any(|line| {
            let event = &line["event"];
            event["kind"] == kind && event["sandbox"] == *sandbox
        })
    };
    let on_modal = |sandbox: &Value| {
        lines.iter().any(|line| {
            let event = &line["event"];
            event["kind"] == "sandbox_deleted"
                && event["sandbox"] == *sandbox
                && event["id"].as_str().is_some_and(|id| id.starts_with("sb-"))
        })
    };
    let created = lines.iter().filter(|line| {
        let event = &line["event"];
        event["kind"] == "sandbox_created" && on_modal(&event["sandbox"])
    });
    let created: Vec<&Value> = created.collect();
    if created.is_empty() {
        return vec!["the log records no create of a sandbox on Modal".to_owned()];
    }
    let companions = |line: &Value| logged("container_met", &line["event"]["sandbox"]);
    (created.into_iter())
        .filter_map(|line| create_unphased(line, assets, companions(line)))
        .collect()
}

/// What is wrong with `line`, a create on Modal in the run log, if anything. Its phases are to be
/// those that the helper times, in order, each within the create. They hold [`ASSETS`] if its
/// sandbox holds `assets`, and [`COMPANIONS`] last if it holds `companions`. One begins after the
/// create did, as the helper's interpreter starts first, unless the helper's clock and this one
/// differ.
fn create_unphased(line: &Value, assets: bool, companions: bool) -> Option<String> {
    let event = &line["event"];
    let wrong = |what: &str| {
        format!(
            "sandbox {}'s create on Modal {what}: {line}",
            event["sandbox"]
        )
    };
    let phases: &[Value] = event["phases"].as_array().map_or(&[], Vec::as_slice);
    let names: Vec<&str> = phases
        .iter()
        .filter_map(|phase| phase["name"].as_str())
        .collect();
    let mut expected = PHASES.to_vec();
    if assets {
        expected.push(ASSETS);
    }
    let held = match (assets, companions) {
        (false, false) => "holds no companions",
        (true, false) => "holds assets and no companions",
        (false, true) => "holds companions",
        (true, true) => "holds assets and companions",
    };
    if companions {
        expected.push(COMPANIONS);
    }
    if names != expected {
        return Some(wrong(&format!(
            "has the phases {names:?}: expected {expected:?}, as its sandbox {held}"
        )));
    }
    let (Some(began), Some(returned)) = (ns(&event["began_ns"]), ns(&line["observed_ns"])) else {
        return Some(wrong("has no times"));
    };
    let mut starts = Vec::new();
    for phase in phases {
        let (Some(start), Some(took)) = (ns(&phase["began_ns"]), ns(&phase["took_ns"])) else {
            return Some(wrong("has a phase without its times"));
        };
        if start < began || start.saturating_add(took) > returned {
            return Some(wrong("has a phase outside the create"));
        }
        starts.push(start);
    }
    if !starts.is_sorted() {
        return Some(wrong("has phases that begin out of order"));
    }
    if starts.iter().all(|start| *start == began) {
        return Some(wrong(
            "has every phase at its start: expected the helper's clock to be this one",
        ));
    }
    None
}

/// What is wrong in `values`, the result of a run of `examples/companions`. That is a fact that it
/// says doesn't hold, with what its commands said. It is also exchanges between containers whose
/// median took [`BRIDGE_MEDIAN_MS`] or longer.
pub(crate) fn uncompanioned(values: &[Value]) -> Vec<String> {
    let value = |name: &str| {
        (values.iter())
            .find(|value| value["name"] == name)
            .map_or(&Value::Null, |value| &value["value"])
    };
    let facts = [
        (
            "users",
            "a container's command didn't run as the user that it declares",
        ),
        (
            "main_reaches",
            "`main` didn't reach each companion by the companion's name",
        ),
        (
            "companions_reach",
            "a companion didn't reach `main`, or the companion before it, by its name",
        ),
        (
            "internet_unreached",
            "a companion's TCP connection to a public address didn't fail as expected",
        ),
    ];
    let mut failed = Vec::new();
    for (fact, wrong) in facts {
        match value(fact) {
            Value::Bool(true) => {}
            Value::Bool(false) => {
                let said = value("unexpected");
                failed.push(format!("{wrong}: its commands said {said}"));
            }
            other => failed.push(format!(
                "the result's `{fact}` is {other}: expected true or false, as the example returns"
            )),
        }
    }
    let bridge = value("bridge_ms");
    match (bridge["median"].as_f64(), bridge["exchanges"].as_u64()) {
        (Some(median), Some(1..)) if median < BRIDGE_MEDIAN_MS => {}
        (Some(median), Some(1..)) => failed.push(format!(
            "the median exchange between `main` and a companion took {median} ms: expected under \
             {BRIDGE_MEDIAN_MS} ms, as on a network inside the sandbox"
        )),
        _ => failed.push(format!(
            "the exchanges between `main` and a companion weren't timed: the result's `bridge_ms` \
             is {bridge}"
        )),
    }
    failed
}

/// What is wrong in `values`, a run's result that counts the facts that hold as `score`, of `of`.
/// That is a fact that doesn't hold, or a result that counts none.
pub(crate) fn unscored(values: &[Value]) -> Vec<String> {
    let value = |name: &str| {
        (values.iter())
            .find(|value| value["name"] == name)
            .map_or(&Value::Null, |value| &value["value"])
    };
    match (value("score").as_u64(), value("of").as_u64()) {
        (Some(score), Some(of @ 1..)) if score == of => Vec::new(),
        (Some(score), Some(of @ 1..)) => {
            let shown: Vec<String> = (values.iter())
                .map(|value| {
                    format!(
                        "{}={}",
                        value["name"].as_str().unwrap_or_default(),
                        value["value"]
                    )
                })
                .collect();
            vec![format!(
                "the result scored {score} of {of}: expected every fact to hold: {}",
                shown.join(" ")
            )]
        }
        (score, of) => vec![format!(
            "the result's `score` is {score:?} of {of:?}: expected a count of the facts that \
             hold, of one or more"
        )],
    }
}

fn ns(value: &Value) -> Option<u64> {
    value.as_str().and_then(|ns| ns.parse::<u64>().ok())
}
