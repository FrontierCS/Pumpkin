//! The workflow IR and its parsing, tested through the crate's public API against a complete
//! workflow, [`WORKFLOW`].
//! Also placing the workflow's roles on providers, in classes of sandbox, the JSON of ops'
//! replies, which `sandboxctl` prints, the sandbox protocol's hellos, and the in-sandbox contract.
//!
//! The workflow stays whole, as the IR's golden fixture: `round_trip` compares a round trip of
//! all of it, and every other IR case is a change to it (`fixture`, `fixture_with`).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};

use expect_test::{Expect, expect};
use schema::ir;
use schema::sandbox::op::{
    self, CommandEnd, CommandStatus, LeaseRemaining, LeaseState, LeaseStatusReply, OpReply,
    TransferEnd, TransferStatus,
};
use schema::sandbox::{
    ExitStatus, Hello, MAX_OP_PAYLOAD_BYTES, MAX_TRANSFER_BYTES, RegistrationGrant, RelativePath,
    ToSandbox, command_env, command_stderr, command_stdin, command_stdout, evidence_dir, frame,
    pumpkin_dir, pumpkin_subdirs, result_file, slot_dir, slot_payload,
};
use schema::workflow::{Class, Offers, Placement, UsdPerHour, Workflow};
use serde_json::{Value, json};
use test_support::WORKFLOW;

const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn fixture() -> Value {
    serde_json::from_str(WORKFLOW).unwrap()
}

/// [`WORKFLOW`] with `value` set at `pointer`, whose parent is an object.
fn fixture_with(pointer: &str, value: Value) -> Value {
    let mut ir = fixture();
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    let parent = ir.pointer_mut(parent).unwrap().as_object_mut().unwrap();
    parent.insert(key.to_owned(), value);
    ir
}

/// Checks what becomes of each case against `expect`, a line each.
fn check<C: Case>(cases: impl IntoIterator<Item = C>, expect: &Expect) {
    let actual: Vec<String> = cases.into_iter().map(Case::outcome).collect();
    expect.assert_eq(&actual.join("\n"));
}

/// A case whose outcome [`check`] shows.
trait Case {
    fn outcome(self) -> String;
}

/// An IR, which parses, or fails to deserialize, or breaks rules, each at a place that resolves
/// in the IR's JSON.
impl Case for Value {
    fn outcome(self) -> String {
        match serde_json::from_value::<ir::Workflow>(self) {
            Err(error) => error.to_string(),
            Ok(ir) => match Workflow::parse(&ir) {
                Ok(_) => "parsed".to_owned(),
                Err(errors) => {
                    let json = serde_json::to_value(&ir).unwrap();
                    for error in errors.as_slice() {
                        assert!(error.at.resolve(&json).is_some(), "{error} resolves");
                    }
                    errors.to_string()
                }
            },
        }
    }
}

/// An IR placed on [`offered`]: each role, where it is placed, with its class's startup, or why
/// the IR fails to parse or be placed.
struct Placing(Value);

impl Case for Placing {
    fn outcome(self) -> String {
        let ir = serde_json::from_value::<ir::Workflow>(self.0).unwrap();
        let offered = offered();
        let placed = Workflow::parse(&ir)
            .and_then(|workflow| workflow.place(&offered, |classes| classes))
            .map_err(|errors| errors.to_string());
        let (placed, providers) = match placed {
            Ok(placed) => placed,
            Err(error) => return error,
        };
        let roles = placed.workflow().roles().map(|(role, sandbox)| {
            let name = &sandbox.name;
            let Placement {
                provider,
                class,
                declared: _,
            } = placed.placement(role);
            let startup = placed.startup(role);
            format!("{name}, on `{provider}` in `{class}`, whose starts take at most {startup:?}")
        });
        let providers = providers
            .iter()
            .map(|(provider, _)| format!("`{provider}`"));
        let roles = roles.collect::<Vec<String>>().join("; ");
        format!(
            "{roles}; providers {}",
            providers.collect::<Vec<_>>().join(", ")
        )
    }
}

/// The train judge, needing what the case gives, placed on [`offered`] beside roles that fit
/// its smallest class: where it is placed, or why it can't be.
struct Needing(Value);

impl Case for Needing {
    fn outcome(self) -> String {
        let mut ir = fixture();
        ir["roles"][1]["needs"] = self.0.clone();
        let ir = serde_json::from_value::<ir::Workflow>(ir).unwrap();
        let offered = offered();
        let placed = Workflow::parse(&ir)
            .unwrap()
            .place(&offered, |classes| classes);
        let at = match placed {
            Ok((placed, _)) => {
                let (judge, _) = placed.workflow().roles().nth(1).unwrap();
                let placement = placed.placement(judge);
                format!("`{}` `{}`", placement.provider, placement.class)
            }
            Err(errors) => errors.to_string(),
        };
        format!("{}: {at}", self.0)
    }
}

/// The classes that two providers offer, as an EC2 envelope and a Modal workspace might: on
/// `aws`, `judge`, `agent` and `judge_memory`, the last two alike, `judge_60`, and `ice_lake`,
/// which states its CPU's model; and on `gpus`, `h100` and `h100_4`, with GPUs.
fn offered() -> BTreeMap<ir::Name, BTreeMap<ir::Name, Class>> {
    let name = |name: &str| ir::Name::try_from(name.to_owned()).unwrap();
    let flags = |flags: &[&str]| flags.iter().map(|flag| valid(flag)).collect();
    let h100 = |count| {
        let model = valid("NVIDIA H100 80GB HBM3");
        let count = NonZeroU32::new(count).unwrap();
        Some(ir::Gpu { count, model })
    };
    let offers = |cpus: u64, memory_gib: u64| Offers {
        cpu_millis: NonZeroU64::new(cpus * 1000).unwrap(),
        memory_bytes: NonZeroU64::new(memory_gib << 30).unwrap(),
        gpu: None,
        cpu_features: flags(&["avx2", "sse4_2"]),
        cpu_model: None,
    };
    let class = |provider: &str, class: &str, offers, startup_seconds| {
        let declared = Class {
            offers,
            startup_seconds: NonZeroU64::new(startup_seconds).unwrap(),
            usd_per_hour: None,
        };
        (name(provider), name(class), declared)
    };
    let avx512 = flags(&["avx2", "avx512f", "sse4_2"]);
    let ice_lake = Offers {
        cpu_features: avx512.clone(),
        cpu_model: Some(valid(XEON)),
        ..offers(4, 16)
    };
    let classes = [
        class("aws", "judge", offers(8, 16), 60),
        class("aws", "agent", offers(8, 64), 30),
        class("aws", "judge_memory", offers(8, 64), 60),
        class(
            "aws",
            "judge_60",
            Offers {
                cpu_features: avx512,
                ..offers(64, 128)
            },
            60,
        ),
        class("aws", "ice_lake", ice_lake, 60),
        class(
            "gpus",
            "h100",
            Offers {
                gpu: h100(1),
                cpu_features: BTreeSet::new(),
                ..offers(8, 64)
            },
            600,
        ),
        class(
            "gpus",
            "h100_4",
            Offers {
                gpu: h100(4),
                cpu_features: BTreeSet::new(),
                ..offers(32, 256)
            },
            600,
        ),
    ];
    let mut offered: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    for (provider, class, declared) in classes {
        offered.entry(provider).or_default().insert(class, declared);
    }
    offered
}

/// The CPU model that [`offered`]'s `ice_lake` states.
const XEON: &str = "Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz";

/// An op's reply as JSON, which reads as the reply shown and writes back as itself, or fails to
/// read.
struct Reply(&'static str);

impl Case for Reply {
    fn outcome(self) -> String {
        match serde_json::from_str::<OpReply>(self.0) {
            Err(error) => error.to_string(),
            Ok(reply) => match serde_json::to_string(&reply).unwrap() {
                json if json == self.0 => format!("{reply:?}"),
                json => format!("{reply:?}, written back as {json}"),
            },
        }
    }
}

/// A hello as JSON, which reads as the hello shown and writes back as itself, or is refused.
struct HelloJson(&'static str);

impl Case for HelloJson {
    fn outcome(self) -> String {
        match serde_json::from_str::<Hello>(self.0) {
            Err(error) => error.to_string(),
            Ok(hello) => match serde_json::to_string(&hello).unwrap() {
                json if json == self.0 => format!("{hello:?}"),
                json => format!("{hello:?}, written back as {json}"),
            },
        }
    }
}

/// Two grants' digits, and whether the grants are equal.
struct Grants(&'static str, &'static str);

impl Case for Grants {
    fn outcome(self) -> String {
        let grant = |digits: &str| RegistrationGrant::try_from(digits.to_owned()).unwrap();
        let equal = grant(self.0) == grant(self.1);
        format!("{} == {}: {equal}", self.0, self.1)
    }
}

/// The bytes a peer sent before its connection ended, and what reading a frame of them gives.
struct Frame(&'static [u8]);

impl Case for Frame {
    fn outcome(self) -> String {
        match frame::read::<Value>(&mut &*self.0) {
            Ok(read) => format!("{:?}: {read:?}", self.0),
            Err(error) => format!("{:?}: {:?}", self.0, error.kind()),
        }
    }
}

/// `value`, one of the IR's checked values.
fn valid<T: TryFrom<String, Error = String>>(value: &str) -> T {
    T::try_from(value.to_owned()).unwrap()
}

/// A relative path below an absolute one, as a role's commands find a path of the in-sandbox
/// contract below `pumpkin_dir()`.
struct Joined(ir::AbsolutePath, RelativePath);

impl Case for Joined {
    fn outcome(self) -> String {
        self.0.join(&self.1).to_string()
    }
}

/// A command's environment as its IR gives it, and as the command runs with it.
struct CommandEnv(Value);

impl Case for CommandEnv {
    fn outcome(self) -> String {
        let env: ir::Env = serde_json::from_value(self.0.clone()).unwrap();
        let runs = serde_json::to_string(&command_env(&env)).unwrap();
        format!("{} runs as {runs}", self.0)
    }
}

#[test]
fn placement() {
    let gib = |gib: u64| gib << 30;
    let h100 = |count: u32| json!({ "count": count, "model": "NVIDIA H100 80GB HBM3" });
    let small = |more: Value| {
        let mut needs = json!({ "cpu_millis": 2000, "memory_bytes": gib(2) });
        needs
            .as_object_mut()
            .unwrap()
            .extend(more.as_object().unwrap().clone());
        Needing(needs)
    };
    check(
        [
            small(json!({})),
            small(json!({ "cpu_millis": 8000, "memory_bytes": gib(16) })),
            small(json!({ "cpu_millis": 8001 })),
            small(json!({ "cpu_millis": 60_000, "memory_bytes": gib(100) })),
            small(json!({ "memory_bytes": gib(32) })),
            small(json!({ "cpu_features": ["avx512f"] })),
            small(json!({ "cpu_model": XEON })),
            small(json!({ "gpu": h100(1) })),
            small(json!({ "gpu": h100(4), "cpu_millis": 16_000, "memory_bytes": gib(128) })),
        ],
        &expect![[r#"
            {"cpu_millis":2000,"memory_bytes":2147483648}: `aws` `judge`
            {"cpu_millis":8000,"memory_bytes":17179869184}: `aws` `judge`
            {"cpu_millis":8001,"memory_bytes":2147483648}: `aws` `judge_60`
            {"cpu_millis":60000,"memory_bytes":107374182400}: `aws` `judge_60`
            {"cpu_millis":2000,"memory_bytes":34359738368}: `aws` `agent`
            {"cpu_features":["avx512f"],"cpu_millis":2000,"memory_bytes":2147483648}: `aws` `judge_60`
            {"cpu_millis":2000,"cpu_model":"Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz","memory_bytes":2147483648}: `aws` `ice_lake`
            {"cpu_millis":2000,"gpu":{"count":1,"model":"NVIDIA H100 80GB HBM3"},"memory_bytes":2147483648}: `gpus` `h100`
            {"cpu_millis":16000,"gpu":{"count":4,"model":"NVIDIA H100 80GB HBM3"},"memory_bytes":137438953472}: `gpus` `h100_4`"#]],
    );
    check(
        [
            small(json!({ "cpu_millis": 128_000 })),
            small(json!({ "memory_bytes": gib(512) })),
            small(json!({ "cpu_features": ["amx_tile", "avx512f"] })),
            small(json!({ "cpu_model": XEON, "cpu_millis": 8000 })),
            small(json!({ "cpu_model": "AMD EPYC 7R13 Processor" })),
            small(json!({ "gpu": h100(2) })),
        ],
        &expect![[r#"
            {"cpu_millis":128000,"memory_bytes":2147483648}: /roles/1/needs: `train_judge` fits no class of sandbox, as `aws`'s `agent` offers 8000 thousandths of a CPU, fewer than its 128000; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 128000; `aws`'s `judge` offers 8000 thousandths of a CPU, fewer than its 128000; `aws`'s `judge_60` offers 64000 thousandths of a CPU, fewer than its 128000; `aws`'s `judge_memory` offers 8000 thousandths of a CPU, fewer than its 128000; `gpus`'s `h100` offers 8000 thousandths of a CPU, fewer than its 128000; `gpus`'s `h100_4` offers 32000 thousandths of a CPU, fewer than its 128000: offer a class that meets its needs, or change its needs
            {"cpu_millis":2000,"memory_bytes":549755813888}: /roles/1/needs: `train_judge` fits no class of sandbox, as `aws`'s `agent` offers 68719476736 bytes of memory, fewer than its 549755813888; `aws`'s `ice_lake` offers 17179869184 bytes of memory, fewer than its 549755813888; `aws`'s `judge` offers 17179869184 bytes of memory, fewer than its 549755813888; `aws`'s `judge_60` offers 137438953472 bytes of memory, fewer than its 549755813888; `aws`'s `judge_memory` offers 68719476736 bytes of memory, fewer than its 549755813888; `gpus`'s `h100` offers 68719476736 bytes of memory, fewer than its 549755813888; `gpus`'s `h100_4` offers 274877906944 bytes of memory, fewer than its 549755813888: offer a class that meets its needs, or change its needs
            {"cpu_features":["amx_tile","avx512f"],"cpu_millis":2000,"memory_bytes":2147483648}: /roles/1/needs: `train_judge` fits no class of sandbox, as `aws`'s `agent` lacks the CPU flags `amx_tile`, `avx512f`, which it needs; `aws`'s `ice_lake` lacks the CPU flags `amx_tile`, which it needs; `aws`'s `judge` lacks the CPU flags `amx_tile`, `avx512f`, which it needs; `aws`'s `judge_60` lacks the CPU flags `amx_tile`, which it needs; `aws`'s `judge_memory` lacks the CPU flags `amx_tile`, `avx512f`, which it needs; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs none; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs none: offer a class that meets its needs, or change its needs
            {"cpu_millis":8000,"cpu_model":"Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz","memory_bytes":2147483648}: /roles/1/needs: `train_judge` fits no class of sandbox, as `aws`'s `agent` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 8000; `aws`'s `judge` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `aws`'s `judge_60` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `aws`'s `judge_memory` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs none; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs none: offer a class that meets its needs, or change its needs
            {"cpu_millis":2000,"cpu_model":"AMD EPYC 7R13 Processor","memory_bytes":2147483648}: /roles/1/needs: `train_judge` fits no class of sandbox, as `aws`'s `agent` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `ice_lake` states the CPU model `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `judge` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `judge_60` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `judge_memory` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs none; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs none: offer a class that meets its needs, or change its needs
            {"cpu_millis":2000,"gpu":{"count":2,"model":"NVIDIA H100 80GB HBM3"},"memory_bytes":2147483648}: /roles/1/needs: `train_judge` fits no class of sandbox, as `aws`'s `agent` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `ice_lake` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge_60` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge_memory` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs 2 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs 2 × `NVIDIA H100 80GB HBM3`: offer a class that meets its needs, or change its needs"#]],
    );
    let mut unplaced = fixture();
    unplaced["roles"][0]["needs"]["gpu"] = h100(8);
    unplaced["roles"][2]["needs"]["cpu_millis"] = json!(64_001);
    check(
        [Placing(fixture()), Placing(unplaced)],
        &expect![[r#"
            agent, on `aws` in `agent`, whose starts take at most 30s; train_judge, on `aws` in `judge`, whose starts take at most 60s; final_judge, on `aws` in `judge`, whose starts take at most 60s; providers `aws`
            /roles/0/needs: `agent` fits no class of sandbox, as `aws`'s `agent` has no GPU, and it needs 8 × `NVIDIA H100 80GB HBM3`; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 8000; `aws`'s `judge` offers 17179869184 bytes of memory, fewer than its 34359738368; `aws`'s `judge_60` has no GPU, and it needs 8 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge_memory` has no GPU, and it needs 8 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs 8 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs 8 × `NVIDIA H100 80GB HBM3`: offer a class that meets its needs, or change its needs
            /roles/2/needs: `final_judge` fits no class of sandbox, as `aws`'s `agent` offers 8000 thousandths of a CPU, fewer than its 64001; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 64001; `aws`'s `judge` offers 8000 thousandths of a CPU, fewer than its 64001; `aws`'s `judge_60` offers 64000 thousandths of a CPU, fewer than its 64001; `aws`'s `judge_memory` offers 8000 thousandths of a CPU, fewer than its 64001; `gpus`'s `h100` offers 8000 thousandths of a CPU, fewer than its 64001; `gpus`'s `h100_4` offers 32000 thousandths of a CPU, fewer than its 64001: offer a class that meets its needs, or change its needs"#]],
    );
}

/// A class's price as JSON, and how it writes back, or why it is refused.
struct Price(Value);

impl Case for Price {
    fn outcome(self) -> String {
        match serde_json::from_value::<UsdPerHour>(self.0.clone()) {
            Ok(price) => format!("{} writes as {}", self.0, json!(price)),
            Err(error) => error.to_string(),
        }
    }
}

#[test]
fn prices() {
    check(
        [
            Price(json!(0)),
            Price(json!(2)),
            Price(json!(0.3672)),
            Price(json!(0.000_001)),
            Price(json!(1_000_000)),
            Price(json!(0.000_000_4)),
            Price(json!(0.123_456_7)),
            Price(json!(-0.000_001)),
            Price(json!(1_000_000.000_001)),
            Price(json!("0.3672")),
        ],
        &expect![[r#"
            0 writes as 0.0
            2 writes as 2.0
            0.3672 writes as 0.3672
            1e-6 writes as 1e-6
            1000000 writes as 1000000.0
            4e-7 writes as 0.0
            0.1234567 writes as 0.123457
            -0.000001 is not a price in dollars an hour: give one from 0 to 1000000, such as 0.3672
            1000000.000001 is not a price in dollars an hour: give one from 0 to 1000000, such as 0.3672
            invalid type: string "0.3672", expected f64"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the replies and the snapshot"
)]
fn op_replies() {
    check(
        [
            Reply(
                r#"{"kind":"ops","remaining_seconds":17940,"resources":{"cpu_millis":2000,"memory_bytes":4294967296},"grants":[{"kind":"judge","name":"train","max_bytes":4194304,"calls":100,"cumulative_seconds":12000,"call_timeout_seconds":600},{"kind":"submit","name":"submit","max_bytes":4194304}]}"#,
            ),
            Reply(r#"{"kind":"judge","job":1,"remaining":{"calls":99,"seconds":12000}}"#),
            Reply(
                r#"{"kind":"status","job":1,"state":"queued","remaining":{"calls":99,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","remaining":{"calls":99,"seconds":12000}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"completed","returned":{"metrics":{"solved":3},"raw_score":3},"scored":true,"remaining":{"calls":99,"seconds":11993}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":2,"state":"completed","returned":{"failure":"wrong answer","raw_score":null},"scored":false,"remaining":{"calls":98,"seconds":11990}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":3,"state":"errored","failure":{"code":"judge_timeout","message":"the judge ran past its timeout"},"remaining":{"calls":97,"seconds":0}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":4,"state":"errored","failure":{"code":"invalid_result","message":"the score at `/raw_score` is not a number: write a number there, or null"},"remaining":{"calls":96,"seconds":0}}"#,
            ),
            Reply(r#"{"kind":"submit","state":"stored","remaining_seconds":120}"#),
            Reply(r#"{"kind":"submit","state":"accepted","remaining_seconds":60}"#),
            Reply(r#"{"kind":"refused","message":"`train` has no calls left: it allows 100"}"#),
            Reply(
                r#"{"kind":"lease","lease":0,"remaining":{"instances":1,"instance_seconds":900,"cumulative_seconds":1200,"commands":null}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":0,"state":"running","elapsed_seconds":40,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"command","command":0,"end":null,"stdout":"","stderr":""},"remaining":{"instances":1,"instance_seconds":899,"cumulative_seconds":1199,"commands":31}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":140,"busy_seconds":79,"idle_seconds":47,"failure":null,"last":{"kind":"command","command":1,"end":{"exited":{"code":0}},"stdout":"GPU 0\n","stderr":""},"remaining":{"instances":1,"instance_seconds":800,"cumulative_seconds":null,"commands":null}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":1,"state":"expired","elapsed_seconds":930,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"command","command":2,"end":"timed_out","stdout":"","stderr":""},"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":0,"commands":null}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":0,"state":"running","elapsed_seconds":41,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"transfer","transfer":1,"end":null},"remaining":{"instances":1,"instance_seconds":898,"cumulative_seconds":1198,"commands":30}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":42,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"transfer","transfer":1,"end":{"moved":{"entries":12,"bytes":40960}}},"remaining":{"instances":1,"instance_seconds":897,"cumulative_seconds":1197,"commands":30}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":0,"state":"ready","elapsed_seconds":43,"busy_seconds":0,"idle_seconds":0,"failure":null,"last":{"kind":"transfer","transfer":2,"end":{"failed":"putting the tree at /work/repo in the runner failed: /work/repo: File exists (os error 17): the tree's root becomes /work/repo, which must not exist"}},"remaining":{"instances":1,"instance_seconds":896,"cumulative_seconds":1196,"commands":29}}"#,
            ),
            Reply(
                r#"{"kind":"lease_status","lease":2,"state":"failed","elapsed_seconds":61,"busy_seconds":0,"idle_seconds":0,"failure":"the runner's sandbox didn't start: ResourceExhaustedError","last":null,"remaining":{"instances":2,"instance_seconds":0,"cumulative_seconds":null,"commands":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","returned":{},"remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"completed","remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"errored","returned":{},"failure":{"code":"judge_error","message":"the judge failed"},"remaining":{"calls":null,"seconds":null}}"#,
            ),
        ],
        &expect![[r#"
            Ops(OpsReply { remaining_seconds: 17940, resources: Resources { cpu_millis: 2000, memory_bytes: 4294967296 }, grants: [Judge(JudgeGrant { name: Name("train"), max_bytes: 4194304, calls: Some(100), cumulative_seconds: Some(12000), call_timeout_seconds: 600 }), Submit(SubmitGrant { name: Name("submit"), max_bytes: 4194304 })] })
            Judge(JudgeReply { job: 1, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Queued, remaining: Remaining { calls: Some(99), seconds: None } })
            Status(StatusReply { job: 1, state: Running, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Completed { returned: Returned({"metrics": Object {"solved": Number(3)}, "raw_score": Number(3)}), scored: true }, remaining: Remaining { calls: Some(99), seconds: Some(11993) } })
            Status(StatusReply { job: 2, state: Completed { returned: Returned({"failure": String("wrong answer"), "raw_score": Null}), scored: false }, remaining: Remaining { calls: Some(98), seconds: Some(11990) } })
            Status(StatusReply { job: 3, state: Errored { failure: Failure { code: JudgeTimeout, message: "the judge ran past its timeout" } }, remaining: Remaining { calls: Some(97), seconds: Some(0) } })
            Status(StatusReply { job: 4, state: Errored { failure: Failure { code: InvalidResult, message: "the score at `/raw_score` is not a number: write a number there, or null" } }, remaining: Remaining { calls: Some(96), seconds: Some(0) } })
            Submit(SubmitReply { state: Stored, remaining_seconds: 120 })
            Submit(SubmitReply { state: Accepted, remaining_seconds: 60 })
            Refused { message: "`train` has no calls left: it allows 100" }
            Lease(LeaseReply { lease: 0, remaining: LeaseRemaining { instances: 1, instance_seconds: 900, cumulative_seconds: Some(1200), commands: None } })
            LeaseStatus(LeaseStatusReply { lease: 0, state: Running, elapsed_seconds: 40, busy_seconds: 0, idle_seconds: 0, failure: None, last: Some(Command(CommandStatus { command: 0, end: None, stdout: "", stderr: "" })), remaining: LeaseRemaining { instances: 1, instance_seconds: 899, cumulative_seconds: Some(1199), commands: Some(31) } })
            LeaseStatus(LeaseStatusReply { lease: 0, state: Ready, elapsed_seconds: 140, busy_seconds: 79, idle_seconds: 47, failure: None, last: Some(Command(CommandStatus { command: 1, end: Some(Exited(Code(0))), stdout: "GPU 0\n", stderr: "" })), remaining: LeaseRemaining { instances: 1, instance_seconds: 800, cumulative_seconds: None, commands: None } })
            LeaseStatus(LeaseStatusReply { lease: 1, state: Expired, elapsed_seconds: 930, busy_seconds: 0, idle_seconds: 0, failure: None, last: Some(Command(CommandStatus { command: 2, end: Some(TimedOut), stdout: "", stderr: "" })), remaining: LeaseRemaining { instances: 2, instance_seconds: 0, cumulative_seconds: Some(0), commands: None } })
            LeaseStatus(LeaseStatusReply { lease: 0, state: Running, elapsed_seconds: 41, busy_seconds: 0, idle_seconds: 0, failure: None, last: Some(Transfer(TransferStatus { transfer: 1, end: None })), remaining: LeaseRemaining { instances: 1, instance_seconds: 898, cumulative_seconds: Some(1198), commands: Some(30) } })
            LeaseStatus(LeaseStatusReply { lease: 0, state: Ready, elapsed_seconds: 42, busy_seconds: 0, idle_seconds: 0, failure: None, last: Some(Transfer(TransferStatus { transfer: 1, end: Some(Moved { entries: 12, bytes: 40960 }) })), remaining: LeaseRemaining { instances: 1, instance_seconds: 897, cumulative_seconds: Some(1197), commands: Some(30) } })
            LeaseStatus(LeaseStatusReply { lease: 0, state: Ready, elapsed_seconds: 43, busy_seconds: 0, idle_seconds: 0, failure: None, last: Some(Transfer(TransferStatus { transfer: 2, end: Some(Failed("putting the tree at /work/repo in the runner failed: /work/repo: File exists (os error 17): the tree's root becomes /work/repo, which must not exist")) })), remaining: LeaseRemaining { instances: 1, instance_seconds: 896, cumulative_seconds: Some(1196), commands: Some(29) } })
            LeaseStatus(LeaseStatusReply { lease: 2, state: Failed, elapsed_seconds: 61, busy_seconds: 0, idle_seconds: 0, failure: Some("the runner's sandbox didn't start: ResourceExhaustedError"), last: None, remaining: LeaseRemaining { instances: 2, instance_seconds: 0, cumulative_seconds: None, commands: None } })
            a job that hasn't ended has no result: give it none of `returned`, `scored` and `failure`
            a completed job has a result: give it `returned` and `scored`, and no `failure`
            an errored job has no result: give it `failure`, and neither `returned` nor `scored`"#]],
    );
}

/// An output's end, as a lease's status gives it ([`op::output_tail`]): how many characters it
/// keeps, and how many bytes they take as JSON writes them.
struct Tail(&'static str, Vec<u8>);

impl Case for Tail {
    fn outcome(self) -> String {
        let Self(name, output) = self;
        let tail = op::output_tail(&output);
        let json = serde_json::to_string(&tail).unwrap().len() - 2;
        format!(
            "{name}: {} characters, {json} bytes as JSON",
            tail.chars().count()
        )
    }
}

/// A lease's status with every field at its largest, `last` as given, in a reply's frame: how
/// many bytes the frame's message takes, or why it can't be written.
struct Largest(op::Last);

impl Case for Largest {
    fn outcome(self) -> String {
        let reason = "\u{1}".repeat(op::REASON_BYTES);
        let status = LeaseStatusReply {
            lease: u64::MAX,
            state: LeaseState::Starting,
            elapsed_seconds: u64::MAX,
            busy_seconds: u64::MAX,
            idle_seconds: u64::MAX,
            failure: Some(reason),
            last: Some(self.0),
            remaining: LeaseRemaining {
                instances: u32::MAX,
                instance_seconds: u64::MAX,
                cumulative_seconds: Some(u64::MAX),
                commands: Some(u64::MAX),
            },
        };
        let message = ToSandbox::OpReply {
            op: u32::MAX,
            reply: OpReply::LeaseStatus(status),
        };
        match frame::write(&mut Vec::new(), &message, &[]) {
            Ok(()) => format!("{} bytes", serde_json::to_vec(&message).unwrap().len()),
            Err(error) => error.to_string(),
        }
    }
}

#[test]
fn lease_status_bound() {
    let worst = || op::output_tail(&vec![1; 2 * op::OUTPUT_TAIL_BYTES]);
    check(
        [
            Tail("text", b"x".repeat(2 * op::OUTPUT_TAIL_BYTES)),
            Tail("short", b"GPU 0\n".to_vec()),
            Tail("control bytes", vec![1; 2 * op::OUTPUT_TAIL_BYTES]),
            Tail("quotes", b"\"".repeat(2 * op::OUTPUT_TAIL_BYTES)),
            Tail("invalid UTF-8", vec![0xff; 2 * op::OUTPUT_TAIL_BYTES]),
            Tail(
                "a character cut",
                format!("{}x", "é".repeat(op::OUTPUT_TAIL_BYTES)).into_bytes(),
            ),
        ],
        &expect![[r#"
            text: 21504 characters, 21504 bytes as JSON
            short: 6 characters, 7 bytes as JSON
            control bytes: 3584 characters, 21504 bytes as JSON
            quotes: 10752 characters, 21504 bytes as JSON
            invalid UTF-8: 7168 characters, 21504 bytes as JSON
            a character cut: 10752 characters, 21503 bytes as JSON"#]],
    );
    check(
        [
            Largest(op::Last::Command(CommandStatus {
                command: u64::MAX,
                end: Some(CommandEnd::Exited(ExitStatus::Signal(i32::MIN))),
                stdout: worst(),
                stderr: worst(),
            })),
            Largest(op::Last::Transfer(TransferStatus {
                transfer: u64::MAX,
                end: Some(TransferEnd::Failed("\u{1}".repeat(op::REASON_BYTES))),
            })),
        ],
        &expect![[r#"
            61951 bytes
            37333 bytes"#]],
    );
}

#[test]
fn hellos() {
    check(
        [
            HelloJson(r#"{"version":0}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899aabbccddeeff"}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899AABBCCDDEEFF"}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899aabbccddee"}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899aabbccddeeff00"}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899aabbccddeefg"}"#),
            HelloJson(r#"{"version":0,"gpus":["NVIDIA A100-SXM4-80GB","NVIDIA H100 80GB HBM3"]}"#),
            HelloJson(r#"{"version":0,"gpus":["NVIDIA A100\u0000"]}"#),
        ],
        &expect![[r#"
            Hello { grant: None, gpus: None }
            Hello { grant: Some(RegistrationGrant(..)), gpus: None }
            the hello isn't of sandbox protocol 0's form: the registration grant isn't 32 lowercase hexadecimal digits: present the one the gateway issued for the sandbox
            the hello isn't of sandbox protocol 0's form: the registration grant isn't 32 lowercase hexadecimal digits: present the one the gateway issued for the sandbox
            the hello isn't of sandbox protocol 0's form: the registration grant isn't 32 lowercase hexadecimal digits: present the one the gateway issued for the sandbox
            the hello isn't of sandbox protocol 0's form: the registration grant isn't 32 lowercase hexadecimal digits: present the one the gateway issued for the sandbox
            Hello { grant: None, gpus: Some([GpuModel("NVIDIA A100-SXM4-80GB"), GpuModel("NVIDIA H100 80GB HBM3")]) }
            the hello isn't of sandbox protocol 0's form: `NVIDIA A100\0` is not a GPU's model: use one as `nvidia-smi` names it, such as `NVIDIA H100 80GB HBM3`"#]],
    );
    check(
        [
            Grants(
                "00112233445566778899aabbccddeeff",
                "00112233445566778899aabbccddeeff",
            ),
            Grants(
                "00112233445566778899aabbccddeeff",
                "10112233445566778899aabbccddeeff",
            ),
            Grants(
                "00112233445566778899aabbccddeeff",
                "00112233445566778899aabbccddeefe",
            ),
            Grants(
                "00112233445566778899aabbccddeeff",
                "01102233445566778899aabbccddeeff",
            ),
        ],
        &expect![[r#"
            00112233445566778899aabbccddeeff == 00112233445566778899aabbccddeeff: true
            00112233445566778899aabbccddeeff == 10112233445566778899aabbccddeeff: false
            00112233445566778899aabbccddeeff == 00112233445566778899aabbccddeefe: false
            00112233445566778899aabbccddeeff == 01102233445566778899aabbccddeeff: false"#]],
    );
    // A connection that ends between frames ends the session; one that ends within a frame's
    // length is cut.
    check(
        [Frame(b""), Frame(b"\0\0")],
        &expect![[r#"
        []: None
        [0, 0]: UnexpectedEof"#]],
    );
}

#[test]
fn in_sandbox_contract() {
    let slot = ir::Name::try_from("payload".to_owned()).unwrap();
    let paths = pumpkin_subdirs().into_iter().chain([
        slot_dir(&slot),
        slot_payload(&slot),
        result_file(),
        evidence_dir(),
        command_stdout(),
        command_stderr(),
        command_stdin(),
    ]);
    let root = ir::AbsolutePath::try_from("/".to_owned()).unwrap();
    let paths = paths.map(|path| Joined(pumpkin_dir(), path));
    check(
        paths.chain([Joined(root, result_file())]),
        &expect![[r#"
            /pumpkin/in
            /pumpkin/out
            /pumpkin/home
            /pumpkin/log
            /pumpkin/in/payload
            /pumpkin/in/payload/raw
            /pumpkin/out/result.json
            /pumpkin/out/evidence
            /pumpkin/log/command.stdout
            /pumpkin/log/command.stderr
            /pumpkin/in/command.stdin
            /out/result.json"#]],
    );
    let envs = [
        json!([]),
        json!(["MODE=train"]),
        json!(["MODE=train", "HOME=/workspace"]),
    ];
    check(
        envs.map(CommandEnv),
        &expect![[r#"
        [] runs as ["HOME=/pumpkin/home"]
        ["MODE=train"] runs as ["HOME=/pumpkin/home","MODE=train"]
        ["MODE=train","HOME=/workspace"] runs as ["MODE=train","HOME=/workspace"]"#]],
    );
}

#[test]
fn round_trip() {
    let ir: ir::Workflow = serde_json::from_str(WORKFLOW).unwrap();
    // Whatever writes the IR serializes these types, so each must read back what it wrote.
    assert_eq!(
        serde_json::to_value(&ir).unwrap(),
        fixture(),
        "a round trip"
    );
    let workflow = Workflow::parse(&ir).unwrap();
    let name = |role| workflow.sandbox(role).name.as_str();
    assert_eq!(name(workflow.agent().role), "agent");
    assert_eq!(workflow.submit_grant().name.as_str(), "submit");
    assert_eq!(name(workflow.score().slot.judge().role()), "final_judge");
    let route = workflow.sandbox(workflow.agent().role).model_route.as_ref();
    let apis: Vec<ir::Api> = route.unwrap().as_slice().iter().map(|e| e.api).collect();
    assert_eq!(apis, [ir::Api::Responses], "the agent's route");
}

#[test]
fn checked_values() {
    let agent = "/roles/0";
    let command = "/roles/0/runtime/agent/command";
    let result = "/roles/1/runtime/judge/result";
    let entry = &fixture()["roles"][0]["egress"]["model"][0];
    check(
        [
            fixture_with("/name", json!("example task")),
            fixture_with(&format!("{agent}/name"), json!("Agent")),
            fixture_with(&format!("{agent}/name"), json!("bad\nname")),
            fixture_with(&format!("{agent}/name"), json!("../escaped")),
            fixture_with(&format!("{agent}/image"), json!(format!("sha256:{HEX}"))),
            fixture_with(&format!("{agent}/image"), json!("ubuntu:24.04")),
            fixture_with(
                &format!("{agent}/image"),
                json!(format!("ubuntu@sha256:{HEX}")),
            ),
            fixture_with(&format!("{command}/cwd"), json!("workspace")),
            fixture_with(&format!("{command}/cwd"), json!("/workspace/")),
            fixture_with(&format!("{command}/argv"), json!([])),
            fixture_with(&format!("{command}/env"), json!(["MODE"])),
            fixture_with(&format!("{command}/env"), json!(["1ST=value"])),
            fixture_with(
                &format!("{command}/env"),
                json!(["MODE=train", "MODE=eval"]),
            ),
            fixture_with(&format!("{agent}/egress/model/0/models"), json!([])),
            fixture_with(
                &format!("{agent}/egress/model/0/models"),
                json!(["gpt-6-astra", "gpt-6-astra"]),
            ),
            fixture_with(&format!("{agent}/egress/model"), json!([])),
            fixture_with(&format!("{agent}/egress/model"), json!([entry, entry])),
            fixture_with(&format!("{result}/score"), json!("raw_score")),
            fixture_with(&format!("{result}/score"), json!("/raw~2score")),
            fixture_with(&format!("{result}/returns"), json!(["/metrics", ""])),
            fixture_with(&format!("{result}/returns"), json!(["/metrics/"])),
            fixture_with(&format!("{result}/returns"), json!(["/x/a~1b", "/a~1b"])),
            fixture_with(
                "/roles/2/runtime/judge/slots/0/max_bytes",
                json!(MAX_OP_PAYLOAD_BYTES + 1),
            ),
        ],
        &expect![[r#"
            `example task` is not a valid workflow name: use 1 to 128 characters from A-Z, a-z, 0-9, `.`, `_`, `-` and `/`
            `Agent` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            `bad\nname` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            `../escaped` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            parsed
            `ubuntu:24.04` is not an image's manifest digest: name the image by that digest alone, `sha256:` and 64 lowercase hexadecimal digits
            `ubuntu@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef` is not an image's manifest digest: name the image by that digest alone, `sha256:` and 64 lowercase hexadecimal digits
            `workspace` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
            `/workspace/` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
            the command is empty: it needs a program
            `MODE` is not an environment entry: write `NAME=value`
            `1ST` is not a valid environment variable name: use A-Z, a-z, 0-9 and `_`, not starting with a digit
            `MODE` is set more than once: set each name once
            no models: a model route allows at least one
            `gpt-6-astra` is listed more than once: list each model once
            no APIs: a model route has an entry for one at least
            the API `responses` has more than one entry: give each API one, which lists every model its requests may name
            `raw_score` is not a JSON pointer: write `/` before each token, `~0` for `~` and `~1` for `/`, or nothing for the whole document
            `/raw~2score` is not a JSON pointer: write `/` before each token, `~0` for `~` and `~1` for `/`, or nothing for the whole document
            the empty pointer names the whole result, with no key: point to a field
            `/metrics/` would return the empty key: point to a field with a name
            `/x/a~1b` and `/a~1b` would both return `a/b`: return each key once
            /roles/2/runtime/judge/slots/0/max_bytes: slot `payload`'s `max_bytes` is 67108865, over the op payload limit of 67108864: give at most that"#]],
    );
    assert_eq!(valid::<ir::Image>(&format!("sha256:{HEX}")).hex(), HEX);
}

#[test]
fn needs_values() {
    let needs = "/roles/0/needs";
    let mut labelled = fixture();
    let role = labelled["roles"][0].as_object_mut().unwrap();
    let resources = role.remove("needs").unwrap();
    role.insert("resources".to_owned(), resources);
    role.insert("placement".to_owned(), json!("agent"));
    let gpu = |count: u32, model: &str| json!({ "count": count, "model": model });
    let every = json!({
        "cpu_millis": 4000,
        "memory_bytes": 17_179_869_184_u64,
        "gpu": gpu(1, "NVIDIA H100 80GB HBM3"),
        "cpu_features": ["avx512bw", "avx512dq", "avx512f"],
        "cpu_model": XEON,
    });
    check(
        [
            fixture_with("/ir_version", json!("pumpkin.ir/v1")),
            labelled,
            fixture_with(needs, every),
            fixture_with(&format!("{needs}/gpu"), gpu(0, "NVIDIA H100 80GB HBM3")),
            fixture_with(&format!("{needs}/gpu"), gpu(1, "")),
            fixture_with(&format!("{needs}/cpu_features"), json!(["AVX512F"])),
            fixture_with(&format!("{needs}/cpu_features"), json!([""])),
            fixture_with(&format!("{needs}/cpu_model"), json!("")),
            fixture_with(&format!("{needs}/cpu_model"), json!("Xeon\n")),
            fixture_with(&format!("{needs}/disk_bytes"), json!(1u64 << 37)),
        ],
        &expect![[r#"
            unknown variant `pumpkin.ir/v1`, expected `pumpkin.ir/v0`
            unknown field `placement`, expected one of `name`, `image`, `user`, `needs`, `runtime`, `egress`, `grants`
            parsed
            invalid value: integer `0`, expected a nonzero u32
            `` is not a GPU's model: use one as `nvidia-smi` names it, such as `NVIDIA H100 80GB HBM3`
            `AVX512F` is not a CPU's flag: use one as `/proc/cpuinfo` names it, such as `avx512f`
            `` is not a CPU's flag: use one as `/proc/cpuinfo` names it, such as `avx512f`
            `` is not a CPU's model: use one as `/proc/cpuinfo` names it, such as `AMD EPYC 7R13 Processor`
            `Xeon\n` is not a CPU's model: use one as `/proc/cpuinfo` names it, such as `AMD EPYC 7R13 Processor`
            unknown field `disk_bytes`, expected one of `cpu_millis`, `memory_bytes`, `gpu`, `cpu_features`, `cpu_model`"#]],
    );
}

#[test]
fn agent_files() {
    let agent = "/roles/0/runtime/agent";
    let dir = |path: &str| json!({ "path": path, "mode": "0755" });
    let file = |path: &str| json!({ "path": path, "mode": "0644", "content": "" });
    let mut broken = fixture();
    let runtime = &mut broken["roles"][0]["runtime"]["agent"];
    runtime["dirs"] = json!([
        dir("/workspace/a"),
        dir("/workspace/a"),
        dir("/workspace/b/d")
    ]);
    runtime["files"] = json!([file("/workspace/a"), file("/workspace/b")]);
    let path = format!("{agent}/files/0/path");
    check(
        [
            fixture_with(&format!("{agent}/dirs/0/mode"), json!("0777")),
            fixture_with(&format!("{agent}/dirs/0/mode"), json!("0800")),
            fixture_with(&format!("{agent}/dirs/0/mode"), json!("4755")),
            fixture_with(&format!("{agent}/dirs/0/mode"), json!("755")),
            fixture_with(&path, json!("/pumpkinx/config.toml")),
            fixture_with(&path, json!("/")),
            fixture_with(&path, json!("/pumpkin/in/command.stdin")),
            fixture_with(&path, json!("/.pumpkin/bin/sandboxctl")),
            fixture_with(&path, json!("/harness")),
            broken,
        ],
        &expect![[r#"
            parsed
            `0800` is not a mode: write `0` and three octal digits, such as `0644`
            `4755` is not a mode: write `0` and three octal digits, such as `0644`
            `755` is not a mode: write `0` and three octal digits, such as `0644`
            parsed
            `/` holds the whole sandbox: name a path below it
            `/pumpkin/in/command.stdin` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
            `/.pumpkin/bin/sandboxctl` is in `/.pumpkin`, which holds Pumpkin's own files: name a path outside it
            `/harness` is in `/harness`, which holds Pumpkin's own files: name a path outside it
            /roles/0/runtime/agent/dirs/1/path: `/workspace/a` is also the path of /roles/0/runtime/agent/dirs/0: put each path once
            /roles/0/runtime/agent/dirs/2/path: `/workspace/b/d` is below `/workspace/b`, the file at /roles/0/runtime/agent/files/1: put it in a directory
            /roles/0/runtime/agent/files/0/path: `/workspace/a` is also the path of /roles/0/runtime/agent/dirs/0: put each path once"#]],
    );
}

#[test]
fn broken_references() {
    let mut ir = fixture();
    let train_judge = ir["roles"][1].clone();
    ir["roles"].as_array_mut().unwrap().push(train_judge);
    let payload = ir["roles"][2]["runtime"]["judge"]["slots"][0].clone();
    ir["roles"][2]["runtime"]["judge"]["slots"]
        .as_array_mut()
        .unwrap()
        .push(payload);
    let grants = &mut ir["roles"][0]["grants"];
    let train = grants[0].clone();
    grants.as_array_mut().unwrap().push(train);
    grants[0]["judge"]["role"] = json!("ghost");
    grants[2]["judge"]["slot"] = json!("submission");
    ir["score"]["role"] = json!("agent");
    check(
        [ir],
        &expect![[r#"
            /roles/3/name: `train_judge` also names /roles/1: give each a unique name
            /roles/2/runtime/judge/slots/1/name: `payload` also names /roles/2/runtime/judge/slots/0: give each a unique name
            /roles/0/grants/0/judge/role: no role is named `ghost`: name one listed in `roles`
            /roles/0/grants/2/judge/slot: `train_judge` has no slot named `submission`: name one of its slots
            /roles/0/grants/2/judge/name: `train` also names /roles/0/grants/0/judge: give each a unique name
            /score/role: `agent` is an agent: name a judge"#]],
    );
}

/// [`WORKFLOW`] with a runner, `gpu`, of an image of its own, that the agent leases under grant
/// `gpu`, before its submit grant.
fn leasing() -> Value {
    let mut ir = fixture();
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu");
    runner["image"] = json!(format!("sha256:{}", "3".repeat(64)));
    runner["runtime"] = json!({ "runner": { "cwd": "/work", "env": ["CUDA=1"] } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "lease": {
        "name": "gpu", "role": "gpu", "instances": 2, "instance_seconds": 900,
        "cumulative_seconds": 1200, "commands": 32, "transfer_bytes": 1_048_576,
    } });
    ir["roles"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    ir
}

#[test]
fn broken_leases() {
    let mut ir = leasing();
    ir["roles"][3]["egress"] = ir["roles"][0]["egress"].clone();
    ir["roles"][3]["grants"] = json!([{ "submit": { "name": "resubmit" } }]);
    let mut judged = leasing();
    judged["roles"][0]["grants"][1]["lease"]["role"] = json!("train_judge");
    let mut twice = leasing();
    let mut again = twice["roles"][0]["grants"][1].clone();
    again["lease"]["name"] = json!("again");
    twice["roles"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .push(again);
    let mut unleased = leasing();
    unleased["roles"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let mut judge_image = leasing();
    judge_image["roles"][3]["image"] = judge_image["roles"][2]["image"].clone();
    let mut ghost = leasing();
    ghost["roles"][0]["grants"][1]["lease"]["role"] = json!("ghost");
    let mut scored = leasing();
    scored["score"]["role"] = json!("gpu");
    let mut agent = leasing();
    agent["roles"][0]["grants"][1]["lease"]["role"] = json!("agent");
    let mut huge = leasing();
    huge["roles"][0]["grants"][1]["lease"]["transfer_bytes"] = json!(MAX_TRANSFER_BYTES + 1);
    let mut most = leasing();
    most["roles"][0]["grants"][1]["lease"]["transfer_bytes"] = json!(MAX_TRANSFER_BYTES);
    check(
        [
            leasing(),
            ir,
            judged,
            twice,
            unleased,
            judge_image,
            ghost,
            scored,
            agent,
            huge,
            most,
        ],
        &expect![[r#"
            parsed
            /roles/3/egress/model: `gpu` is a runner, and runners have no model route: remove it
            /roles/3/grants: `gpu` is a runner, and runners hold no grants: remove them
            /roles/0/grants/1/lease/role: `train_judge` is a judge: name a runner
            /roles/3: `gpu` is a runner that no lease grant names: name it from one
            /roles/0/grants/3/lease/role: `gpu` is named by `agent`'s lease grant `gpu` and `agent`'s `again`: name each runner from one grant
            /roles/3: `gpu` is a runner that no lease grant names: name it from one
            /roles/0/grants/1/lease/role: `gpu` runs judge `train_judge`'s image, sha256:2222222222222222222222222222222222222222222222222222222222222222: give the runner an image no judge has
            /roles/0/grants/1/lease/role: no role is named `ghost`: name one listed in `roles`
            /roles/3: `gpu` is a runner that no lease grant names: name it from one
            /score/role: `gpu` is a runner: name a judge
            /roles/0/grants/1/lease/role: `agent` is an agent: name a runner
            /roles/3: `gpu` is a runner that no lease grant names: name it from one
            /roles/0/grants/1/lease/transfer_bytes: `gpu`'s `transfer_bytes` is 1073741825, over the transfer limit of 1073741824: give at most that
            parsed"#]],
    );
}

/// [`leasing`]'s workflow, with a runner, `checker`, of the final judge's image, which the final
/// judge leases under its grant `box`.
fn judge_leasing() -> Value {
    let mut ir = leasing();
    let mut checker = ir["roles"][3].clone();
    checker["name"] = json!("checker");
    checker["image"] = ir["roles"][2]["image"].clone();
    ir["roles"].as_array_mut().unwrap().push(checker);
    ir["roles"][2]["grants"] = json!([{ "lease": {
        "name": "box", "role": "checker", "instances": 1, "instance_seconds": 60,
        "transfer_bytes": 64,
    } }]);
    ir
}

#[test]
fn judge_leases() {
    let mut same_image = judge_leasing();
    let image = json!(format!("sha256:{}", "4".repeat(64)));
    same_image["roles"][3]["image"] = image.clone();
    same_image["roles"][4]["image"] = image;
    let mut both = judge_leasing();
    both["roles"][2]["grants"][0]["lease"]["role"] = json!("gpu");
    let mut unheld = judge_leasing();
    unheld["roles"][2]["grants"] = json!([
        { "lease": {
            "name": "box", "role": "train_judge", "instances": 1, "instance_seconds": 60,
            "transfer_bytes": 64,
        } },
        { "submit": { "name": "box" } },
        { "judge": {
            "name": "again", "role": "train_judge", "slot": "payload", "call_timeout_seconds": 1,
        } },
    ]);
    check(
        [judge_leasing(), same_image, both, unheld],
        &expect![[r#"
            parsed
            parsed
            /roles/2/grants/0/lease/role: `gpu` is named by `agent`'s lease grant `gpu` and `final_judge`'s `box`: name each runner from one grant
            /roles/4: `checker` is a runner that no lease grant names: name it from one
            /roles/2/grants/1/submit: `final_judge` is a judge, and judges hold only lease grants: remove it
            /roles/2/grants/2/judge: `final_judge` is a judge, and judges hold only lease grants: remove it
            /roles/2/grants/1/submit/name: `box` also names /roles/2/grants/0/lease: give each a unique name
            /roles/2/grants/0/lease/role: `train_judge` is a judge: name a runner
            /roles/4: `checker` is a runner that no lease grant names: name it from one"#]],
    );
}

#[test]
fn broken_roles() {
    let mut ir = fixture();
    let mut helper = ir["roles"][0].clone();
    helper["name"] = json!("helper");
    ir["roles"].as_array_mut().unwrap().push(helper);
    ir["roles"][2]["egress"] = ir["roles"][0]["egress"].clone();
    ir["roles"][1]["grants"] = json!([{ "submit": { "name": "resubmit" } }]);
    let grants = &mut ir["roles"][0]["grants"];
    let submit = json!({ "submit": { "name": "resubmit" } });
    let mut retrain = grants[0].clone();
    retrain["judge"]["name"] = json!("retrain");
    grants.as_array_mut().unwrap().extend([submit, retrain]);
    grants[0]["judge"]["slot"] = json!("submission");
    ir["score"]["role"] = json!("train_judge");
    let mut none = fixture();
    none["roles"].as_array_mut().unwrap().remove(0);
    let mut unsubmitted = fixture();
    unsubmitted["roles"][0]["grants"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let mut ungranted = fixture();
    ungranted["roles"][0]
        .as_object_mut()
        .unwrap()
        .remove("grants");
    let mut untimed = fixture();
    untimed["roles"][0]["grants"][0]["judge"]
        .as_object_mut()
        .unwrap()
        .remove("call_timeout_seconds");
    check(
        [ir, none, unsubmitted, ungranted, untimed],
        &expect![[r#"
            /roles/1/grants/0/submit: `train_judge` is a judge, and judges hold only lease grants: remove it
            /roles/3/runtime: `helper` is a second agent, after `agent`: a workflow has exactly one
            /roles/0/grants/0/judge/slot: `train_judge` has no slot named `submission`: name one of its slots
            /roles/0/grants/3/judge/role: `train_judge` is named by judge grants `train` and `retrain`: name each judge from one grant
            /roles/0/grants/2/submit: `resubmit` is a second submit grant, after `submit`: give the agent exactly one
            /score/role: `train_judge` is also the role of judge grant `train`: name a judge no grant names
            /roles: no role is an agent: a workflow has exactly one
            /roles/0/grants: no submit grant: give the agent exactly one
            /roles/0: no submit grant: give the agent exactly one
            missing field `call_timeout_seconds`"#]],
    );
}
