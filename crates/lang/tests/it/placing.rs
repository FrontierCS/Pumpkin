//! Placing a workflow's roles, through `Plan::check` and `Workflow::place`. A role that fits no
//! class is refused where its `SourceMap` places the role's needs, in whichever file wrote them.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use expect_test::{Expect, expect};
use lang::{Args, Bounds, Label};
use schema::ir::Name;
use schema::workflow::{Class, Offers};

use crate::{Case, Sources, files, on_stack};

/// Checks against `expect` what placing the workflow of the case that `fixture` writes gives, as
/// `Case::parse` reads the case. The one provider, `aws`, offers one class, `small`, of 2 CPUs and
/// 8 GiB.
#[track_caller]
fn check(fixture: &str, expect: &Expect) {
    let files = files();
    let shown = on_stack(|| {
        let (_, case) = Case::parse(fixture, &files, Bounds::WORKFLOW);
        let mut sources = Sources {
            held: &case.held,
            files: &files,
        };
        let entry = Label::parse(&format!("//{}", case.entry)).unwrap();
        let args = Args::parse([]).unwrap();
        let plan = lang::evaluate(&entry, &mut sources, &args, Bounds::WORKFLOW).unwrap();
        let (workflow, source_map) = plan.check(|_, _| Vec::new()).unwrap();
        let small = Class {
            offers: Offers {
                cpu_millis: NonZeroU64::new(2000).unwrap(),
                memory_bytes: NonZeroU64::new(8 << 30).unwrap(),
                gpu: None,
                cpu_features: BTreeSet::new(),
                cpu_model: None,
                companions: false,
                assets: false,
            },
            startup_seconds: NonZeroU64::new(60).unwrap(),
            usd_per_hour: None,
        };
        let name = |name: &str| Name::try_from(name.to_owned()).unwrap();
        let classes = BTreeMap::from([(name("small"), small)]);
        let providers = BTreeMap::from([(name("aws"), classes)]);
        match workflow.place(providers, |classes| classes) {
            Ok(_) => "placed".to_owned(),
            Err(errors) => (errors.in_source(|at| source_map.place(at))).to_string(),
        }
    });
    expect.assert_eq(&shown);
}

// A misfit is at the needs where they were written: in the file that a role's needs are loaded
// from, or in the entry.
#[test]
fn misfits() {
    check(
        r#"
#- //misfit/task.star
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")
load("//misfit/needs.star", "GPU")

workflow(
    name = "neutral/misfit",
    roles = [
        AGENT | {"needs": GPU},
        FINAL_TARGET | {"needs": {"cpu_millis": 4000, "memory_bytes": 512 * MiB}},
    ],
    final = FINAL,
    deadline_seconds = 3600,
)
#- //misfit/needs.star
GPU = {
    "cpu_millis": 2000,
    "memory_bytes": 4 * GiB,
    "gpu": {"count": 1, "model": "NVIDIA H100 80GB HBM3"},
}
"#,
        &expect![[r#"
            error: role `agent` fits no class of sandbox
              --> misfit/needs.star:1:7: roles[0].needs: 2 CPUs, 4.0 GiB, 1 × `NVIDIA H100 80GB HBM3`
              = `aws/small`: no GPU
              = help: add a class with 1 × `NVIDIA H100 80GB HBM3`, and at least 2 CPUs and 4.0 GiB
            error: role `final_judge` fits no class of sandbox
              --> misfit/task.star:8:34: roles[1].needs: 4 CPUs, 512.0 MiB
              = `aws/small`: 2 CPUs
              = help: add a class with at least 4 CPUs and 512.0 MiB,
                      or lower the role's `cpu_millis` to 2000, which `aws/small` offers"#]],
    );
}

// An asset that no class holds is refused at the role's assets, in the file that wrote them.
#[test]
fn assets() {
    check(
        r#"
#- //assets/task.star
load("//lib/parts.star", "AGENT", "FINAL_TARGET", "FINAL")
load("//assets/model.star", "MODEL")

workflow(
    name = "neutral/assets",
    roles = [AGENT, FINAL_TARGET | {"assets": MODEL}],
    final = FINAL,
    deadline_seconds = 3600,
)
#- //assets/model.star
CONFIG = {
    "path": "config.json",
    "url": "https://example.com/config.json",
    "size": 963,
    "sha256": "sha256:e2c8d8ee53f9f4d5d0c6a1f3b7a9e0c4d2f1b3a5c7e9f1d3b5a7c9e1f3d5b7a9",
}

MODEL = [{"path": "/opt/model", "files": [CONFIG]}]
"#,
        &expect![[r#"
            error: role `final_judge` declares assets, and no class of sandbox holds them
              --> assets/model.star:8:9: roles[1].assets: 1 asset at `/opt/model`
              = help: declare none,
                      or run it where a class holds them"#]],
    );
}
