//! What `cargo xtask live` reads of the configuration that names the controller, and what it
//! refuses there before any run, and which lines of a run's progress tell a lease ready.

use std::path::Path;

use expect_test::expect;

use super::settings::{Provider, Settings};
use super::{record, wait};

/// A configuration whose controller has four Modal classes, three of them with a GPU, two of which
/// share one, beside a controller at an address.
const CONFIG: &str = r#"[controllers.lab]
address = "lab.example.org:2222"

[controllers.alice]
region = "us-west-1"

[controllers.alice.modal]
profile = "alice"
classes.a100 = { gpu = "A100-40GB", gpu_model = "NVIDIA A100-SXM4-40GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
classes.a100_2 = { gpu = "A100-40GB:2", gpu_model = "NVIDIA A100-SXM4-40GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
classes.h100 = { gpu = "H100!", gpu_model = "NVIDIA H100 80GB HBM3", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 }
classes.cpu = { cpu_millis = 2000, memory_bytes = 4294967296, startup_seconds = 600 }
"#;

// Each case: what the settings are, or the refusal.
#[test]
fn settings() {
    let path = Path::new("/home/alice/.config/pumpkin/pumpkin.toml");
    let a100 = Some("NVIDIA A100-SXM4-40GB");
    let without_modal = CONFIG.split("[controllers.alice.modal]").next().unwrap();
    let several = format!("{CONFIG}\n[controllers.bob]\nregion = \"us-east-1\"\n");
    let gpus_only = CONFIG.replace("classes.cpu", "# classes.cpu");
    let cases = [
        ("ec2", CONFIG, Provider::Ec2, None),
        ("modal", CONFIG, Provider::Modal, a100),
        ("modal without a GPU", CONFIG, Provider::Modal, None),
        (
            "modal with a GPU no class has",
            CONFIG,
            Provider::Modal,
            Some("NVIDIA B200"),
        ),
        (
            "modal with a GPU in every class",
            &gpus_only,
            Provider::Modal,
            a100,
        ),
        (
            "modal without its modal",
            without_modal,
            Provider::Modal,
            a100,
        ),
        (
            "no controller",
            "[providers.local.docker]\n",
            Provider::Ec2,
            None,
        ),
        ("several with a region", &several, Provider::Ec2, None),
    ];
    let lines: Vec<String> = cases
        .into_iter()
        .map(|(case, text, provider, gpu_model)| {
            let parsed = Settings::parse(text, path, provider, gpu_model);
            match parsed {
                Ok(settings) => format!("{case}: {settings:?}"),
                Err(error) => format!("{case}: {error}"),
            }
        })
        .collect();
    expect![[r#"
        ec2: Settings { name: "alice", region: "us-west-1", modal: None }
        modal: Settings { name: "alice", region: "us-west-1", modal: Some(Modal { profile: "alice", gpu_model: "NVIDIA A100-SXM4-40GB" }) }
        modal without a GPU: a live run on Modal needs the model of a GPU that a class of `[controllers.alice.modal]` has, the cheapest best: give one of `NVIDIA A100-SXM4-40GB`, `NVIDIA H100 80GB HBM3` with `--gpu-model`
        modal with a GPU no class has: no class of /home/alice/.config/pumpkin/pumpkin.toml's `[controllers.alice.modal]` has the GPU `NVIDIA B200`: give one of `NVIDIA A100-SXM4-40GB`, `NVIDIA H100 80GB HBM3`
        modal with a GPU in every class: a live run on Modal runs examples/companions on a class without a GPU, and /home/alice/.config/pumpkin/pumpkin.toml's `[controllers.alice.modal]` has none: add one, such as `classes.cpu = { cpu_millis = 2000, memory_bytes = 4294967296, startup_seconds = 600 }`
        modal without its modal: /home/alice/.config/pumpkin/pumpkin.toml names no `[controllers.alice.modal]`, which a live run on Modal needs: name the workspace's profile and classes there, as the README says
        no controller: /home/alice/.config/pumpkin/pumpkin.toml names no `[controllers.<name>]` with a `region`, which a live run goes through: name the one `deploy/aws` made, as the README says
        several with a region: /home/alice/.config/pumpkin/pumpkin.toml names several controllers with a `region`, `alice`, `bob`: keep there the one that a live run goes through"#]].assert_eq(&lines.join("\n"));
}

// Lines that `./pumpkin wait` says, as its own tests pin them.
#[test]
fn lease_ready() {
    let lines = [
        "pumpkin:    0:38 cpu lease 0 of agent 0 ready",
        "pumpkin:    2:06 gpu lease 1 of agent 0 ready with NVIDIA A100-SXM4-80GB",
        "pumpkin:    8:25 gpu lease 3 of agent 0 ready with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)",
        "pumpkin:    0:55 box lease 0 of job 0 ready",
        "pumpkin:    0:39 cpu lease 0 ready",
        "pumpkin:    0:19 cpu lease 0 of agent 0 asked on aws/judge",
        "pumpkin:    1:14 gpu lease 1 of agent 0 ended: closed by its holder",
        "     0:41 gpu lease 1 on modal/a100_40 with NVIDIA A100-SXM4-40GB, 1m41s: charged 1m34s, 0s of it busy, 0 commands; closed as the work phase ended",
        "pumpkin: run 20261114-215915-8388a47f started",
    ];
    let shown: Vec<String> = (lines.iter())
        .map(|line| format!("{}: {line}", wait::lease_ready(line)))
        .collect();
    expect![[r#"
        true: pumpkin:    0:38 cpu lease 0 of agent 0 ready
        true: pumpkin:    2:06 gpu lease 1 of agent 0 ready with NVIDIA A100-SXM4-80GB
        true: pumpkin:    8:25 gpu lease 3 of agent 0 ready with NVIDIA A100 80GB PCIe (its role needs NVIDIA A100-SXM4-80GB)
        false: pumpkin:    0:55 box lease 0 of job 0 ready
        false: pumpkin:    0:39 cpu lease 0 ready
        false: pumpkin:    0:19 cpu lease 0 of agent 0 asked on aws/judge
        false: pumpkin:    1:14 gpu lease 1 of agent 0 ended: closed by its holder
        false:      0:41 gpu lease 1 on modal/a100_40 with NVIDIA A100-SXM4-40GB, 1m41s: charged 1m34s, 0s of it busy, 0 commands; closed as the work phase ended
        false: pumpkin: run 20261114-215915-8388a47f started"#]].assert_eq(&shown.join("\n"));
}

// What a failed `./pumpkin reap` said, then whether its failure is only other runs': of runs
// that it can't reap, or whose provider it can't reach, and of a connection that ended.
#[test]
fn others_failed() {
    let this = "20261114-215915-8388a47f";
    let other = "pumpkin: run 20261114-220102-1c2d3e4f's sandboxes on provider `modal`, if it \
                 left any, can't be removed, as it can't be reached: no token\n\
                 pumpkin: 1 run can't be reaped, as said above: fix what each says, and reap \
                 again\n";
    let among = "pumpkin: run 20261114-220102-1c2d3e4f can't be reaped: a record's file is \
                 missing\n\
                 pumpkin: run 20261114-215915-8388a47f can't be reaped: a record's file is \
                 missing\n\
                 pumpkin: 2 runs can't be reaped, as said above: fix what each says, and reap \
                 again\n";
    let none = "pumpkin: the connection to controller `alice` ended (exit status: 255), as said \
                above: reap again: pumpkin reap\n";
    let cases = [
        ("only another run named", other),
        ("this run named among others", among),
        ("no run named", none),
    ];
    let shown: Vec<String> = (cases.iter())
        .map(|(case, said)| format!("{case}: {}", wait::others_failed(said, this)))
        .collect();
    expect![[r#"
        only another run named: true
        this run named among others: false
        no run named: false"#]]
    .assert_eq(&shown.join("\n"));
}

/// The lines of a log, each an event of `kind` of sandbox `sandbox`, with its other fields
/// `fields`, observed at the time `at` in nanoseconds.
fn log(events: &[(&str, &str, u64, serde_json::Value)]) -> Vec<serde_json::Value> {
    (events.iter().enumerate())
        .map(|(seq, (kind, sandbox, at, fields))| {
            let mut event = serde_json::json!({"kind": kind, "sandbox": sandbox});
            event
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().cloned().unwrap_or_default());
            serde_json::json!({"seq": seq, "observed_ns": at.to_string(), "event": event})
        })
        .collect()
}

/// A create of `sandbox` that began at 100 ns and returned at 900 ns, with a phase of each name
/// of `phases`, one after another.
fn created(sandbox: &str, phases: &[&str]) -> (&'static str, String, u64, serde_json::Value) {
    let phases: Vec<serde_json::Value> = (phases.iter().zip(0_u64..))
        .map(|(name, index)| {
            let began = (200 + 100 * index).to_string();
            serde_json::json!({"name": name, "began_ns": began, "took_ns": "50"})
        })
        .collect();
    let fields = serde_json::json!({"began_ns": "100", "phases": phases});
    ("sandbox_created", sandbox.to_owned(), 900, fields)
}

fn met(container: &str) -> serde_json::Value {
    serde_json::json!({"container": container})
}

// Each case: a log of a create on Modal, then what is wrong with its phases.
#[test]
fn unphased() {
    let deleted = serde_json::json!({"id": "sb-1"});
    let phases = ["lookup", "create", "tunnels", "list"];
    let all_phases = ["lookup", "create", "tunnels", "list", "companions"];
    let mounted = ["lookup", "create", "tunnels", "list", "assets"];
    let shown = |(case, phases, assets, companions): (&str, &[&str], bool, bool)| {
        let (kind, sandbox, at, fields) = created("lease-0", phases);
        let mut events = vec![(kind, sandbox.as_str(), at, fields)];
        if companions {
            events.push(("container_met", "lease-0", 1000, met("main")));
        }
        events.push(("sandbox_deleted", "lease-0", 2000, deleted.clone()));
        let lines = log(&events);
        // The message holds the create's line, which the case's name says.
        let unphased = record::unphased(&lines, assets).join("; ");
        let unphased = unphased.replace(&lines[0].to_string(), "<create>");
        format!("{case}: {unphased}")
    };
    let cases: [(&str, &[&str], bool, bool); 5] = [
        ("with companions", &all_phases, false, true),
        ("with companions, their phase missing", &phases, false, true),
        (
            "without companions, with their phase",
            &all_phases,
            false,
            false,
        ),
        ("with assets", &mounted, true, false),
        ("with assets, their phase missing", &phases, true, false),
    ];
    let shown: Vec<String> = cases.into_iter().map(shown).collect();
    expect![[r#"
        with companions: 
        with companions, their phase missing: sandbox "lease-0"'s create on Modal has the phases ["lookup", "create", "tunnels", "list"]: expected ["lookup", "create", "tunnels", "list", "companions"], as its sandbox holds companions: <create>
        without companions, with their phase: sandbox "lease-0"'s create on Modal has the phases ["lookup", "create", "tunnels", "list", "companions"]: expected ["lookup", "create", "tunnels", "list"], as its sandbox holds no companions: <create>
        with assets: 
        with assets, their phase missing: sandbox "lease-0"'s create on Modal has the phases ["lookup", "create", "tunnels", "list"]: expected ["lookup", "create", "tunnels", "list", "assets"], as its sandbox holds assets and no companions: <create>"#]].assert_eq(&shown.join("\n"));
}

// Each case: a result that counts the facts that hold, then what is wrong in it.
#[test]
fn unscored() {
    let result = |score: serde_json::Value| {
        let values = [("score", score), ("of", 3.into()), ("pinned", true.into())];
        let values = values.map(|(name, value)| serde_json::json!({"name": name, "value": value}));
        values.to_vec()
    };
    let cases = [
        ("every fact holds", result(3.into())),
        ("a fact doesn't hold", result(2.into())),
        ("no score", result(serde_json::Value::Null)),
    ];
    let shown: Vec<String> = (cases.into_iter())
        .map(|(case, values)| format!("{case}: {:?}", record::unscored(&values)))
        .collect();
    expect![[r#"
        every fact holds: []
        a fact doesn't hold: ["the result scored 2 of 3: expected every fact to hold: score=2 of=3 pinned=true"]
        no score: ["the result's `score` is None of Some(3): expected a count of the facts that hold, of one or more"]"#]].assert_eq(&shown.join("\n"));
}

// Each case: a result of `examples/companions`, then what is wrong in it.
#[test]
fn uncompanioned() {
    let result = |users: bool, bridge: serde_json::Value| {
        let facts = [
            ("users", users),
            ("main_reaches", true),
            ("companions_reach", true),
            ("internet_unreached", true),
        ];
        let mut values: Vec<serde_json::Value> = (facts.into_iter())
            .map(|(name, fact)| serde_json::json!({"name": name, "value": fact}))
            .collect();
        values.push(serde_json::json!({"name": "bridge_ms", "value": bridge}));
        let unexpected = if users {
            serde_json::json!({})
        } else {
            serde_json::json!({"c3": "0:0\nmain\nc2\n"})
        };
        values.push(serde_json::json!({"name": "unexpected", "value": unexpected}));
        values
    };
    let timed =
        |median: f64| serde_json::json!({"median": median, "slowest": 3.2, "exchanges": 900});
    let cases = [
        ("every fact holds", result(true, timed(0.7))),
        ("a companion ran as root", result(false, timed(0.7))),
        ("a slow bridge", result(true, timed(300.0))),
        ("no exchange", result(true, serde_json::json!({"said": ""}))),
    ];
    let shown: Vec<String> = (cases.into_iter())
        .map(|(case, values)| format!("{case}: {:?}", record::uncompanioned(&values)))
        .collect();
    expect![[r#"
        every fact holds: []
        a companion ran as root: ["a container's command didn't run as the user that it declares: its commands said {\"c3\":\"0:0\\nmain\\nc2\\n\"}"]
        a slow bridge: ["the median exchange between `main` and a companion took 300 ms: expected under 50 ms, as on a network inside the sandbox"]
        no exchange: ["the exchanges between `main` and a companion weren't timed: the result's `bridge_ms` is {\"said\":\"\"}"]"#]].assert_eq(&shown.join("\n"));
}
