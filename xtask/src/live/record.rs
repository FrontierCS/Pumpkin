//! What `cargo xtask live` finds wrong in a run's record, from its parsed lines and values alone,
//! so that its tests reach no provider.

use serde_json::Value;

/// The phases that the Modal helper times of each create, in order.
const PHASES: [&str; 4] = ["lookup", "create", "tunnels", "list"];

/// The phase that the Modal helper times after [`PHASES`] for a sandbox that holds companions.
const SIDECARS: &str = "sidecars";

/// The longest that the median exchange between two containers of a sandbox may take, in
/// milliseconds.
// Modal's bridge carries such an exchange in about a millisecond, and a link through the gateway
// in hundreds. So only a path inside the sandbox passes.
const BRIDGE_MEDIAN_MS: f64 = 50.0;

/// What is wrong with each create on Modal in `lines`, a run's log. That is a phase missing, out of
/// order or outside its create, or a log without one. A create of a sandbox that holds companions,
/// one whose containers the log says met, is to have [`SIDECARS`] last, and any other none. A Modal
/// sandbox's id starts with `sb-`, and its delete in the log names it.
pub(crate) fn unphased(lines: &[Value]) -> Vec<String> {
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
        .filter_map(|line| create_unphased(line, companions(line)))
        .collect()
}

/// What is wrong with `line`, a create on Modal in the run log, if anything. Its phases are to be
/// those that the helper times, in order, each within the create, with [`SIDECARS`] last if its
/// sandbox holds `companions`. One begins after the create did, as the helper's interpreter starts
/// first, unless the helper's clock and this one differ.
fn create_unphased(line: &Value, companions: bool) -> Option<String> {
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
    let held = if companions {
        expected.push(SIDECARS);
        "holds companions"
    } else {
        "holds none"
    };
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

fn ns(value: &Value) -> Option<u64> {
    value.as_str().and_then(|ns| ns.parse::<u64>().ok())
}
