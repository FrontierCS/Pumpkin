//! What `cargo xtask live` reads of the configuration that names the controller, and what it
//! refuses there before any run, and which lines of a run's progress tell a lease ready.

use std::path::Path;

use expect_test::expect;

use super::settings::{Modal, Provider, Settings};
use super::wait;

const CONFIG: &str = r#"[controller]
name = "alice"
region = "us-west-1"
"#;

// Each case: what the settings are, or the refusal.
#[test]
fn settings() {
    let path = Path::new("/home/alice/.config/pumpkin/pumpkin.toml");
    let a100 = || Some("NVIDIA A100-SXM4-40GB".to_owned());
    let alice = || Some("alice".to_owned());
    let cases = [
        ("ec2", CONFIG, Ok(Provider::Ec2)),
        (
            "modal",
            CONFIG,
            Modal::new(a100(), alice()).map(Provider::Modal),
        ),
        (
            "modal without a GPU",
            CONFIG,
            Modal::new(None, alice()).map(Provider::Modal),
        ),
        (
            "modal without a profile",
            CONFIG,
            Modal::new(a100(), None).map(Provider::Modal),
        ),
        (
            "no [controller]",
            "[providers.local.docker]\n",
            Ok(Provider::Ec2),
        ),
    ];
    let lines: Vec<String> = cases
        .into_iter()
        .map(|(case, text, provider)| {
            let parsed = provider.and_then(|provider| Settings::parse(text, path, provider));
            match parsed {
                Ok(settings) => format!("{case}: {settings:?}"),
                Err(error) => format!("{case}: {error}"),
            }
        })
        .collect();
    expect![[r#"
        ec2: Settings { name: "alice", region: "us-west-1", modal: None }
        modal: Settings { name: "alice", region: "us-west-1", modal: Some(Modal { profile: "alice", gpu_model: "NVIDIA A100-SXM4-40GB" }) }
        modal without a GPU: a live run on Modal needs the model of a GPU that a class of the controller's `modal_classes` has, the cheapest best: give it with `--gpu-model`
        modal without a profile: a live run on Modal needs the profile of Modal's configuration that reaches the controller's workspace, `modal_workspace`, to check that the run left no sandbox there: give it with `--modal-profile`
        no [controller]: /home/alice/.config/pumpkin/pumpkin.toml names no `[controller]`, which a live run goes through: name the one `deploy/aws` made, as the README says"#]].assert_eq(&lines.join("\n"));
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
