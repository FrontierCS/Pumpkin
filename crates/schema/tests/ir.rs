//! The workflow IR and its parsing, tested through the crate's public API against a complete
//! workflow, [`WORKFLOW`].
//!
//! The workflow stays whole, as the IR's golden fixture: every IR case is a change to it
//! (`fixture`, `fixture_with`).

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::{NonZeroU32, NonZeroU64};

use expect_test::{Expect, expect};
use schema::sandbox::op::{
    self, CommandEnd, CommandStatus, LeaseRemaining, LeaseState, LeaseStatusReply, OpReply,
    TransferEnd, TransferStatus,
};
use schema::sandbox::{
    Endpoint, ExitStatus, Hello, MAX_OP_PAYLOAD_BYTES, MAX_TRANSFER_BYTES, RegistrationGrant,
    RelativePath, ToSandbox, command_env, command_stderr, command_stdin, command_stdout,
    pumpkin_dir, pumpkin_subdirs,
};
use schema::workflow::{Class, Offers, Placement, UsdPerHour, Workflow};
use schema::{frame, ir};
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
        serde_json::from_value::<ir::Workflow>(self).outcome()
    }
}

/// An IR as read, from a `Value` or from text.
impl Case for Result<ir::Workflow, serde_json::Error> {
    fn outcome(self) -> String {
        match self {
            Err(error) => error.to_string(),
            Ok(ir) => match Workflow::parse(&ir) {
                Ok(_) => "parsed".to_owned(),
                Err(errors) => {
                    let json = serde_json::to_value(&ir).unwrap();
                    for error in errors.as_slice() {
                        assert!(
                            json.pointer(error.at.as_str()).is_some(),
                            "{error} resolves"
                        );
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

/// The trainer, needing what the case gives, placed on [`offered`] beside roles that fit
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
                let (trainer, _) = placed.workflow().roles().nth(1).unwrap();
                let placement = placed.placement(trainer);
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
            {"cpu_millis":128000,"memory_bytes":2147483648}: /roles/1/needs: `trainer` fits no class of sandbox, as `aws`'s `agent` offers 8000 thousandths of a CPU, fewer than its 128000; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 128000; `aws`'s `judge` offers 8000 thousandths of a CPU, fewer than its 128000; `aws`'s `judge_60` offers 64000 thousandths of a CPU, fewer than its 128000; `aws`'s `judge_memory` offers 8000 thousandths of a CPU, fewer than its 128000; `gpus`'s `h100` offers 8000 thousandths of a CPU, fewer than its 128000; `gpus`'s `h100_4` offers 32000 thousandths of a CPU, fewer than its 128000: offer a class that meets its needs, or change its needs
            {"cpu_millis":2000,"memory_bytes":549755813888}: /roles/1/needs: `trainer` fits no class of sandbox, as `aws`'s `agent` offers 68719476736 bytes of memory, fewer than its 549755813888; `aws`'s `ice_lake` offers 17179869184 bytes of memory, fewer than its 549755813888; `aws`'s `judge` offers 17179869184 bytes of memory, fewer than its 549755813888; `aws`'s `judge_60` offers 137438953472 bytes of memory, fewer than its 549755813888; `aws`'s `judge_memory` offers 68719476736 bytes of memory, fewer than its 549755813888; `gpus`'s `h100` offers 68719476736 bytes of memory, fewer than its 549755813888; `gpus`'s `h100_4` offers 274877906944 bytes of memory, fewer than its 549755813888: offer a class that meets its needs, or change its needs
            {"cpu_features":["amx_tile","avx512f"],"cpu_millis":2000,"memory_bytes":2147483648}: /roles/1/needs: `trainer` fits no class of sandbox, as `aws`'s `agent` lacks the CPU flags `amx_tile`, `avx512f`, which it needs; `aws`'s `ice_lake` lacks the CPU flags `amx_tile`, which it needs; `aws`'s `judge` lacks the CPU flags `amx_tile`, `avx512f`, which it needs; `aws`'s `judge_60` lacks the CPU flags `amx_tile`, which it needs; `aws`'s `judge_memory` lacks the CPU flags `amx_tile`, `avx512f`, which it needs; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs none; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs none: offer a class that meets its needs, or change its needs
            {"cpu_millis":8000,"cpu_model":"Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz","memory_bytes":2147483648}: /roles/1/needs: `trainer` fits no class of sandbox, as `aws`'s `agent` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 8000; `aws`'s `judge` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `aws`'s `judge_60` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `aws`'s `judge_memory` states no CPU model, and it needs `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs none; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs none: offer a class that meets its needs, or change its needs
            {"cpu_millis":2000,"cpu_model":"AMD EPYC 7R13 Processor","memory_bytes":2147483648}: /roles/1/needs: `trainer` fits no class of sandbox, as `aws`'s `agent` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `ice_lake` states the CPU model `Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz`, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `judge` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `judge_60` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `aws`'s `judge_memory` states no CPU model, and it needs `AMD EPYC 7R13 Processor`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs none; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs none: offer a class that meets its needs, or change its needs
            {"cpu_millis":2000,"gpu":{"count":2,"model":"NVIDIA H100 80GB HBM3"},"memory_bytes":2147483648}: /roles/1/needs: `trainer` fits no class of sandbox, as `aws`'s `agent` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `ice_lake` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge_60` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge_memory` has no GPU, and it needs 2 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs 2 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs 2 × `NVIDIA H100 80GB HBM3`: offer a class that meets its needs, or change its needs"#]],
    );
    let mut unplaced = fixture();
    unplaced["roles"][0]["needs"]["gpu"] = h100(8);
    unplaced["roles"][2]["needs"]["cpu_millis"] = json!(64_001);
    check(
        [Placing(fixture()), Placing(unplaced)],
        &expect![[r#"
            agent, on `aws` in `agent`, whose starts take at most 30s; trainer, on `aws` in `judge`, whose starts take at most 60s; final_judge, on `aws` in `judge`, whose starts take at most 60s; providers `aws`
            /roles/0/needs: `agent` fits no class of sandbox, as `aws`'s `agent` has no GPU, and it needs 8 × `NVIDIA H100 80GB HBM3`; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 8000; `aws`'s `judge` offers 17179869184 bytes of memory, fewer than its 34359738368; `aws`'s `judge_60` has no GPU, and it needs 8 × `NVIDIA H100 80GB HBM3`; `aws`'s `judge_memory` has no GPU, and it needs 8 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100` has GPUs, 1 × `NVIDIA H100 80GB HBM3`, and it needs 8 × `NVIDIA H100 80GB HBM3`; `gpus`'s `h100_4` has GPUs, 4 × `NVIDIA H100 80GB HBM3`, and it needs 8 × `NVIDIA H100 80GB HBM3`: offer a class that meets its needs, or change its needs
            /roles/2/needs: `final_judge` fits no class of sandbox, as `aws`'s `agent` offers 8000 thousandths of a CPU, fewer than its 64001; `aws`'s `ice_lake` offers 4000 thousandths of a CPU, fewer than its 64001; `aws`'s `judge` offers 8000 thousandths of a CPU, fewer than its 64001; `aws`'s `judge_60` offers 64000 thousandths of a CPU, fewer than its 64001; `aws`'s `judge_memory` offers 8000 thousandths of a CPU, fewer than its 64001; `gpus`'s `h100` offers 8000 thousandths of a CPU, fewer than its 64001; `gpus`'s `h100_4` offers 32000 thousandths of a CPU, fewer than its 64001: offer a class that meets its needs, or change its needs"#]],
    );
}

/// Where the gateway dials a sandbox that listens: a DNS name and a port, which parse, or are
/// refused.
struct At<'a>(&'a str, u16);

impl Case for At<'_> {
    fn outcome(self) -> String {
        match Endpoint::new(self.0.to_owned(), self.1) {
            Ok(endpoint) => format!("`{endpoint}` parsed"),
            Err(error) => error,
        }
    }
}

#[test]
fn endpoints() {
    let long = format!("{}.example", "a".repeat(64));
    check(
        [
            At("ta-01k9-7434-xyz.w.modal.host", 443),
            At("localhost", 7434),
            At("", 443),
            At("127.0.0.1:443", 443),
            At("127.0.0.1", 443),
            At("1.example", 443),
            At("-edge.example", 443),
            At(&long, 443),
            At("edge..example", 443),
            At("edge.example", 0),
        ],
        &expect![[r#"
            `ta-01k9-7434-xyz.w.modal.host:443` parsed
            `localhost:7434` parsed
            `` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`
            `127.0.0.1:443` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`
            `127.0.0.1` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`
            `1.example:443` parsed
            `-edge.example` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`
            `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.example` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`
            `edge..example` is not a DNS name: give the name the endpoint's certificate is for, such as `ta-1.w.modal.host`
            port 0 of `edge.example` is no endpoint's: give the port it serves"#]],
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
            Price(json!(0.3672)),
            Price(json!(0.000_001)),
            Price(json!(1_000_000)),
            Price(json!(0.000_000_4)),
            Price(json!(0.123_456_7)),
            Price(json!(-0.000_001)),
            Price(json!(1_000_000.000_001)),
        ],
        &expect![[r#"
            0 writes as 0.0
            0.3672 writes as 0.3672
            1e-6 writes as 1e-6
            1000000 writes as 1000000.0
            4e-7 writes as 0.0
            0.1234567 writes as 0.123457
            -0.000001 is not a price in dollars an hour: give one from 0 to 1000000, such as 0.3672
            1000000.000001 is not a price in dollars an hour: give one from 0 to 1000000, such as 0.3672"#]],
    );
}

#[test]
fn op_replies() {
    check(
        [
            Reply(
                r#"{"kind":"status","job":1,"state":"queued","remaining":{"calls":99,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","remaining":{"calls":99,"seconds":12000}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"completed","returned":{"metrics":{"solved":3},"raw_score":3},"remaining":{"calls":99,"seconds":11993}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":2,"state":"completed","returned":{"failure":"wrong answer","raw_score":null},"model_requests_over_budget":1,"remaining":{"calls":98,"seconds":11990}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":3,"state":"errored","failure":{"code":"call_timeout","message":"the call ran past its timeout"},"remaining":{"calls":97,"seconds":0}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":4,"state":"errored","failure":{"code":"call_error","message":"the call's body failed: the run's record says why"},"model_requests_over_budget":3,"remaining":{"calls":96,"seconds":0}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","returned":{},"remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"running","model_requests_over_budget":1,"remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"completed","remaining":{"calls":null,"seconds":null}}"#,
            ),
            Reply(
                r#"{"kind":"status","job":1,"state":"errored","returned":{},"failure":{"code":"call_error","message":"the call's body failed: the run's record says why"},"remaining":{"calls":null,"seconds":null}}"#,
            ),
        ],
        &expect![[r#"
            Status(StatusReply { job: 1, state: Queued, remaining: Remaining { calls: Some(99), seconds: None } })
            Status(StatusReply { job: 1, state: Running, remaining: Remaining { calls: Some(99), seconds: Some(12000) } })
            Status(StatusReply { job: 1, state: Completed { returned: Returned({"metrics": Object {"solved": Number(3)}, "raw_score": Number(3)}), model_requests_over_budget: None }, remaining: Remaining { calls: Some(99), seconds: Some(11993) } })
            Status(StatusReply { job: 2, state: Completed { returned: Returned({"failure": String("wrong answer"), "raw_score": Null}), model_requests_over_budget: Some(1) }, remaining: Remaining { calls: Some(98), seconds: Some(11990) } })
            Status(StatusReply { job: 3, state: Errored { failure: Failure { code: CallTimeout, message: "the call ran past its timeout" }, model_requests_over_budget: None }, remaining: Remaining { calls: Some(97), seconds: Some(0) } })
            Status(StatusReply { job: 4, state: Errored { failure: Failure { code: CallError, message: "the call's body failed: the run's record says why" }, model_requests_over_budget: Some(3) }, remaining: Remaining { calls: Some(96), seconds: Some(0) } })
            a job that hasn't ended has no result: give it neither `returned` nor `failure`
            a job that hasn't ended has no count of model requests over budget yet: give it no `model_requests_over_budget`
            a completed job has a result: give it `returned`, and no `failure`
            an errored job has no result: give it `failure`, and no `returned`"#]],
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
fn hellos_and_frames() {
    check(
        [
            HelloJson(r#"{"version":0}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899aabbccddeeff"}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899AABBCCDDEEFF"}"#),
            HelloJson(r#"{"version":0,"grant":"00112233445566778899aabbccddee"}"#),
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
    let paths =
        pumpkin_subdirs()
            .into_iter()
            .chain([command_stdout(), command_stderr(), command_stdin()]);
    let root = ir::AbsolutePath::try_from("/".to_owned()).unwrap();
    let paths = paths.map(|path| Joined(pumpkin_dir(), path));
    check(
        paths.chain([Joined(root, command_stdin())]),
        &expect![[r#"
            /pumpkin/in
            /pumpkin/out
            /pumpkin/home
            /pumpkin/log
            /pumpkin/log/command.stdout
            /pumpkin/log/command.stderr
            /pumpkin/in/command.stdin
            /in/command.stdin"#]],
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

/// A valid IR, parsed: its agent, its `now` functions, the role of its final call with that call's
/// keywords, and the agent's model route, by API.
struct Parsed(Value);

impl Case for Parsed {
    fn outcome(self) -> String {
        let ir: ir::Workflow = serde_json::from_value(self.0).unwrap();
        let workflow = Workflow::parse(&ir).unwrap();
        let name = |role| workflow.sandbox(role).name.as_str();
        let now = workflow.now_functions().map(|(_, now)| now.name.as_str());
        let route = workflow.agent().model.as_ref().unwrap();
        let apis: Vec<ir::Api> = route.as_slice().iter().map(|entry| entry.api).collect();
        let final_call = workflow.final_call();
        format!(
            "agent `{}`, now functions {:?}, final call on `{}` with keywords {}, route {apis:?}",
            name(workflow.agent().role),
            now.collect::<Vec<_>>(),
            name(final_call.target.role()),
            serde_json::to_string(&final_call.keywords).unwrap(),
        )
    }
}

#[test]
fn parsed() {
    check(
        [
            Parsed(fixture()),
            Parsed(fixture_with("/final/keywords", json!({ "mode": "final" }))),
        ],
        &expect![[r#"
            agent `agent`, now functions ["submit"], final call on `final_judge` with keywords {}, route [Responses]
            agent `agent`, now functions ["submit"], final call on `final_judge` with keywords {"mode":"final"}, route [Responses]"#]],
    );
}

#[test]
fn checked_values() {
    let agent = "/roles/0";
    let command = "/roles/0/runtime/agent/command";
    let route = "/roles/0/runtime/agent/egress/model";
    let entry = &fixture()["roles"][0]["runtime"]["agent"]["egress"]["model"][0];
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
            fixture_with(&format!("{route}/0/models"), json!([])),
            fixture_with(
                &format!("{route}/0/models"),
                json!(["gpt-6-astra", "gpt-6-astra"]),
            ),
            fixture_with(route, json!([])),
            fixture_with(route, json!([entry, entry])),
            fixture_with("/final/payload_max_bytes", json!(MAX_OP_PAYLOAD_BYTES + 1)),
            fixture_with("/final/payload_max_bytes", json!(MAX_OP_PAYLOAD_BYTES)),
        ]
        .into_iter()
        .map(serde_json::from_value),
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
            `gpt-6-astra` is in more than one `responses` entry: list each model in one entry for its API
            /final/payload_max_bytes: the final call's `payload_max_bytes` is 67108865, over the op payload limit of 67108864: give at most that
            parsed"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn header_names() {
    let entry = "/roles/0/runtime/agent/egress/model/0";
    let listed =
        |list: &str, names: &[&str]| fixture_with(&format!("{entry}/{list}"), json!(names));
    let refused = [
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "api-key",
        "x-goog-api-key",
        "x-goog-user-project",
        "ocp-apim-subscription-key",
        "cookie",
        "set-cookie",
        "openai-organization",
        "openai-project",
        "anthropic-organization-id",
        "anthropic-workspace-id",
        "anthropic-user-profile-id",
        "x-amz-security-token",
        "x-amz-date",
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "upgrade",
        "proxy-authenticate",
        "accept-encoding",
        "content-encoding",
    ];
    let mut cases = vec![
        listed(
            "request_headers",
            &["x-codex-window-id", "x-amzn-trace-id", "a0!#$%&'*+-.^_`|~"],
        ),
        listed("request_headers", &[&"x".repeat(64)]),
        listed("request_headers", &[&"x".repeat(65)]),
        listed("request_headers", &[""]),
        listed("request_headers", &["X-Codex-Window-Id"]),
        listed("request_headers", &["x-codex window"]),
        listed("response_headers", &["x-codex:primary"]),
    ];
    cases.extend(refused.map(|name| listed("request_headers", &[name])));
    cases.push(listed("response_headers", &["set-cookie"]));
    check(
        cases,
        &expect![[r#"
            parsed
            parsed
            `xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx` is not a header name: use 1 to 64 characters from a-z, 0-9 and ``!#$%&'*+-.^_`|~``
            `` is not a header name: use 1 to 64 characters from a-z, 0-9 and ``!#$%&'*+-.^_`|~``
            `X-Codex-Window-Id` is not a header name: use 1 to 64 characters from a-z, 0-9 and ``!#$%&'*+-.^_`|~``
            `x-codex window` is not a header name: use 1 to 64 characters from a-z, 0-9 and ``!#$%&'*+-.^_`|~``
            `x-codex:primary` is not a header name: use 1 to 64 characters from a-z, 0-9 and ``!#$%&'*+-.^_`|~``
            `authorization` carries a credential or picks an account, which are the host's alone: list another header
            `proxy-authorization` carries a credential or picks an account, which are the host's alone: list another header
            `x-api-key` carries a credential or picks an account, which are the host's alone: list another header
            `api-key` carries a credential or picks an account, which are the host's alone: list another header
            `x-goog-api-key` carries a credential or picks an account, which are the host's alone: list another header
            `x-goog-user-project` carries a credential or picks an account, which are the host's alone: list another header
            `ocp-apim-subscription-key` carries a credential or picks an account, which are the host's alone: list another header
            `cookie` carries a credential or picks an account, which are the host's alone: list another header
            `set-cookie` carries a credential or picks an account, which are the host's alone: list another header
            `openai-organization` carries a credential or picks an account, which are the host's alone: list another header
            `openai-project` carries a credential or picks an account, which are the host's alone: list another header
            `anthropic-organization-id` carries a credential or picks an account, which are the host's alone: list another header
            `anthropic-workspace-id` picks the workspace that the request runs and is billed in, which is the host's alone: list another header
            `anthropic-user-profile-id` attributes the request to a user profile, on whose behalf it acts, which is the host's alone: list another header
            `x-amz-security-token` signs the request or carries a session's token, which are the host's alone: list another header
            `x-amz-date` signs the request or carries a session's token, which are the host's alone: list another header
            `host` frames the message on one hop, which the route does for each: list another header
            `content-length` frames the message on one hop, which the route does for each: list another header
            `transfer-encoding` frames the message on one hop, which the route does for each: list another header
            `connection` frames the message on one hop, which the route does for each: list another header
            `keep-alive` frames the message on one hop, which the route does for each: list another header
            `proxy-connection` frames the message on one hop, which the route does for each: list another header
            `te` frames the message on one hop, which the route does for each: list another header
            `trailer` frames the message on one hop, which the route does for each: list another header
            `upgrade` frames the message on one hop, which the route does for each: list another header
            `proxy-authenticate` frames the message on one hop, which the route does for each: list another header
            `accept-encoding` sets the body's coding, which the route keeps to `identity`: list another header
            `content-encoding` sets the body's coding, which the route keeps to `identity`: list another header
            `set-cookie` carries a credential or picks an account, which are the host's alone: list another header"#]],
    );
}

#[test]
fn needs_values() {
    let needs = "/roles/0/needs";
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
            fixture_with(needs, every),
            fixture_with(&format!("{needs}/gpu"), gpu(1, "")),
            fixture_with(&format!("{needs}/cpu_features"), json!(["AVX512F"])),
            fixture_with(&format!("{needs}/cpu_features"), json!([""])),
            fixture_with(&format!("{needs}/cpu_model"), json!("")),
            fixture_with(&format!("{needs}/cpu_model"), json!("Xeon\n")),
            fixture_with(&format!("{needs}/disk_bytes"), json!(1u64 << 37)),
        ],
        &expect![[r#"
            parsed
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
            fixture_with(&path, json!("/harness/codex/config.toml")),
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
            parsed
            parsed
            /roles/0/runtime/agent/dirs/1/path: `/workspace/a` is also the path of /roles/0/runtime/agent/dirs/0: put each path once
            /roles/0/runtime/agent/dirs/2/path: `/workspace/b/d` is below `/workspace/b`, the file at /roles/0/runtime/agent/files/1: put it in a directory
            /roles/0/runtime/agent/files/0/path: `/workspace/a` is also the path of /roles/0/runtime/agent/dirs/0: put each path once"#]],
    );
}

#[test]
fn broken_references() {
    let mut ir = fixture();
    let trainer = ir["roles"][1].clone();
    ir["roles"].as_array_mut().unwrap().push(trainer);
    let grants = &mut ir["roles"][0]["runtime"]["agent"]["grants"];
    let train = grants[0].clone();
    grants.as_array_mut().unwrap().push(train);
    grants[0]["job"]["target"] = json!("ghost");
    ir["final"]["target"] = json!("agent");
    check(
        [ir],
        &expect![[r#"
            /roles/3/name: `trainer` also names /roles/1: give each a unique name
            /roles/0/runtime/agent/grants/2/name: `train` also names /roles/0/runtime/agent/grants/0: give each a unique name
            /roles/0/runtime/agent/grants/0/job/target: no role is named `ghost`: name one listed in `roles`
            /final/target: `agent` is an agent: name a target as the final call's target
            /roles/2: `final_judge` is a target that no job function or final call names: name it from one
            /roles/3: `trainer` is a target that no job function or final call names: name it from one"#]],
    );
}

#[test]
fn grant_forms() {
    let with = |change: fn(&mut Value)| {
        let mut ir = fixture();
        change(&mut ir["roles"][0]["runtime"]["agent"]["grants"][0]);
        ir
    };
    check(
        [
            with(|grant| grant["now"] = json!({})),
            with(|grant| *grant = json!({ "name": "train" })),
            with(|grant| {
                grant.as_object_mut().unwrap().remove("name");
            }),
            fixture_with("/final/body/arguments", json!({ "level": { "int": {} } })),
        ],
        &expect![[r#"
            `now` follows `job`, the function's kind: give a grant its `name` and one function
            the grant gives no function: give one, under its kind's key
            missing field `name`
            unknown field `arguments`, expected `file` or `def`"#]],
    );
}

/// [`WORKFLOW`] with a runner, `gpu`, of an image of its own, that the agent leases under grant
/// `gpu`, before its now function.
fn leasing() -> Value {
    let mut ir = fixture();
    let mut runner = ir["roles"][1].clone();
    runner["name"] = json!("gpu");
    runner["image"] = json!(format!("sha256:{}", "3".repeat(64)));
    runner["runtime"] = json!({ "runner": { "cwd": "/work", "env": ["CUDA=1"] } });
    ir["roles"].as_array_mut().unwrap().push(runner);
    let grant = json!({ "name": "gpu", "session": {
        "runner": "gpu", "instances": 2, "instance_seconds": 900,
        "cumulative_seconds": 1200, "commands": 32, "transfer_bytes": 1_048_576,
    } });
    ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .insert(1, grant);
    ir
}

// A now function named `name`, of a body in the entry file.
fn now(name: &str) -> Value {
    json!({ "name": name, "now": {
        "body": { "file": "//task.star", "def": "submit" }, "payload_max_bytes": 4096,
    } })
}

#[test]
fn broken_leases() {
    let mut judged = leasing();
    judged["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["runner"] = json!("final_judge");
    let mut twice = leasing();
    let mut again = twice["roles"][0]["runtime"]["agent"]["grants"][1].clone();
    again["name"] = json!("again");
    twice["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .push(again);
    let mut unleased = leasing();
    unleased["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let mut target_image = leasing();
    target_image["roles"][3]["image"] = target_image["roles"][2]["image"].clone();
    let mut ghost = leasing();
    ghost["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["runner"] = json!("ghost");
    let mut final_on_runner = leasing();
    final_on_runner["final"]["target"] = json!("gpu");
    let mut agent = leasing();
    agent["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["runner"] = json!("agent");
    let mut huge = leasing();
    huge["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["transfer_bytes"] =
        json!(MAX_TRANSFER_BYTES + 1);
    let mut most = leasing();
    most["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["transfer_bytes"] =
        json!(MAX_TRANSFER_BYTES);
    check(
        [
            leasing(),
            judged,
            twice,
            unleased,
            target_image,
            ghost,
            final_on_runner,
            agent,
            huge,
            most,
        ],
        &expect![[r#"
            parsed
            /roles/0/runtime/agent/grants/1/session/runner: `final_judge` is a target: name a runner
            /roles/3: `gpu` is a runner that no session names: name it from one
            /roles/0/runtime/agent/grants/3/session/runner: `gpu` is named by `agent`'s session `gpu` and `agent`'s `again`: name each runner from one session
            /roles/3: `gpu` is a runner that no session names: name it from one
            /roles/0/runtime/agent/grants/1/session/runner: `gpu` runs target `trainer`'s image, sha256:2222222222222222222222222222222222222222222222222222222222222222: give the runner an image no target has
            /roles/0/runtime/agent/grants/1/session/runner: no role is named `ghost`: name one listed in `roles`
            /roles/3: `gpu` is a runner that no session names: name it from one
            /final/target: `gpu` is a runner: name a target as the final call's target
            /roles/2: `final_judge` is a target that no job function or final call names: name it from one
            /roles/0/runtime/agent/grants/1/session/runner: `agent` is an agent: name a runner
            /roles/3: `gpu` is a runner that no session names: name it from one
            /roles/0/runtime/agent/grants/1/session/transfer_bytes: `gpu`'s `transfer_bytes` is 1073741825, over the transfer limit of 1073741824: give at most that
            parsed"#]],
    );
}

/// [`leasing`]'s workflow, with a target, `verifier`, of an image of its own, of the agent's job
/// function `check`.
fn calling() -> Value {
    let mut ir = leasing();
    let mut verifier = ir["roles"][3].clone();
    verifier["name"] = json!("verifier");
    verifier["image"] = json!(format!("sha256:{}", "5".repeat(64)));
    verifier["runtime"] = json!({ "target": { "cwd": "/work" } });
    ir["roles"].as_array_mut().unwrap().push(verifier);
    let function = json!({ "name": "check", "job": {
        "target": "verifier", "body": { "file": "//lib/verify.star", "def": "check" },
        "keywords": { "metrics": ["passed"] }, "payload_max_bytes": 1024, "calls": 3,
        "call_timeout_seconds": 60,
    } });
    ir["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .push(function);
    ir
}

/// [`calling`]'s workflow, whose target holds the agent's model route and a session, `box`, of a
/// runner of an image of its own.
fn target_holding() -> Value {
    let mut ir = calling();
    let mut boxed = ir["roles"][3].clone();
    boxed["name"] = json!("box");
    boxed["image"] = json!(format!("sha256:{}", "6".repeat(64)));
    ir["roles"].as_array_mut().unwrap().push(boxed);
    let mut session = ir["roles"][0]["runtime"]["agent"]["grants"][1].clone();
    session["name"] = json!("box");
    session["session"]["runner"] = json!("box");
    ir["roles"][4]["runtime"]["target"]["sessions"] = json!([session]);
    ir["roles"][4]["runtime"]["target"]["egress"] =
        ir["roles"][0]["runtime"]["agent"]["egress"].clone();
    ir
}

#[test]
fn job_functions() {
    let with = |change: fn(&mut Value)| {
        let mut ir = calling();
        change(&mut ir["roles"][0]["runtime"]["agent"]["grants"][3]);
        ir
    };
    let mut leased = calling();
    leased["roles"][0]["runtime"]["agent"]["grants"][1]["session"]["runner"] = json!("verifier");
    let mut same_image = calling();
    same_image["roles"][4]["image"] = same_image["roles"][3]["image"].clone();
    let mut twice = calling();
    let mut again = twice["roles"][0]["runtime"]["agent"]["grants"][3].clone();
    again["name"] = json!("again");
    twice["roles"][0]["runtime"]["agent"]["grants"]
        .as_array_mut()
        .unwrap()
        .push(again);
    let mut uncalled = calling();
    let grants = &mut uncalled["roles"][0]["runtime"]["agent"]["grants"];
    grants.as_array_mut().unwrap().remove(3);
    let mut shared = calling();
    shared["final"]["target"] = json!("verifier");
    check(
        [
            calling(),
            with(|grant| grant["job"]["target"] = json!("final_judge")),
            with(|grant| grant["job"]["target"] = json!("gpu")),
            with(|grant| grant["job"]["payload_max_bytes"] = json!(MAX_OP_PAYLOAD_BYTES + 1)),
            with(|grant| grant["job"]["body"]["file"] = json!("lib/verify.star")),
            leased,
            same_image,
            twice,
            uncalled,
            target_holding(),
            shared,
        ],
        &expect![[r#"
            parsed
            /final/target: `final_judge` is the final call's target and `check`'s: give the final call a role no job names
            /roles/4: `verifier` is a target that no job function or final call names: name it from one
            /roles/0/runtime/agent/grants/3/job/target: `gpu` is a runner: name a target as a job's target
            /roles/4: `verifier` is a target that no job function or final call names: name it from one
            /roles/0/runtime/agent/grants/3/job/payload_max_bytes: `check`'s `payload_max_bytes` is 67108865, over the op payload limit of 67108864: give at most that
            `lib/verify.star` is no label: start it with `//`, the root
            /roles/0/runtime/agent/grants/1/session/runner: `verifier` is a target: name a runner
            /roles/3: `gpu` is a runner that no session names: name it from one
            /roles/0/runtime/agent/grants/1/session/runner: `gpu` runs target `verifier`'s image, sha256:3333333333333333333333333333333333333333333333333333333333333333: give the runner an image no target has
            /roles/0/runtime/agent/grants/4/job/target: `verifier` is the target of job functions `check` and `again`: name each target from one function
            /roles/4: `verifier` is a target that no job function or final call names: name it from one
            parsed
            /final/target: `verifier` is the final call's target and `check`'s: give the final call a role no job names
            /roles/2: `final_judge` is a target that no job function or final call names: name it from one"#]],
    );
}

/// [`leasing`]'s workflow, with a runner, `checker`, of the image of the final call's target,
/// which the target leases under its grant `box`.
fn target_leasing() -> Value {
    let mut ir = leasing();
    let mut checker = ir["roles"][3].clone();
    checker["name"] = json!("checker");
    checker["image"] = ir["roles"][2]["image"].clone();
    ir["roles"].as_array_mut().unwrap().push(checker);
    ir["roles"][2]["runtime"]["target"]["sessions"] = json!([{ "name": "box", "session": {
        "runner": "checker", "instances": 1, "instance_seconds": 60, "transfer_bytes": 64,
    } }]);
    ir
}

#[test]
fn target_leases() {
    let mut same_image = target_leasing();
    let image = json!(format!("sha256:{}", "4".repeat(64)));
    same_image["roles"][3]["image"] = image.clone();
    same_image["roles"][4]["image"] = image;
    let mut both = target_leasing();
    both["roles"][2]["runtime"]["target"]["sessions"][0]["session"]["runner"] = json!("gpu");
    let mut twice = target_leasing();
    let grants = &mut twice["roles"][2]["runtime"]["target"]["sessions"];
    let mut again = grants[0].clone();
    again["session"]["runner"] = json!("trainer");
    grants.as_array_mut().unwrap().insert(0, again);
    check(
        [target_leasing(), same_image, both, twice],
        &expect![[r#"
            parsed
            parsed
            /roles/2/runtime/target/sessions/0/session/runner: `gpu` is named by `agent`'s session `gpu` and `final_judge`'s `box`: name each runner from one session
            /roles/4: `checker` is a runner that no session names: name it from one
            /roles/2/runtime/target/sessions/1/name: `box` also names /roles/2/runtime/target/sessions/0: give each a unique name
            /roles/2/runtime/target/sessions/0/session/runner: `trainer` is a target: name a runner"#]],
    );
}

#[test]
fn broken_roles() {
    let mut ir = fixture();
    let mut helper = ir["roles"][0].clone();
    helper["name"] = json!("helper");
    ir["roles"].as_array_mut().unwrap().push(helper);
    ir["roles"][2]["runtime"]["target"]["egress"] =
        ir["roles"][0]["runtime"]["agent"]["egress"].clone();
    let grants = &mut ir["roles"][0]["runtime"]["agent"]["grants"];
    let mut submit = now("resubmit");
    submit["now"]["payload_max_bytes"] = json!(67_108_865);
    let mut retrain = grants[0].clone();
    retrain["name"] = json!("retrain");
    grants.as_array_mut().unwrap().extend([submit, retrain]);
    ir["final"]["target"] = json!("trainer");
    let mut none = fixture();
    none["roles"].as_array_mut().unwrap().remove(0);
    check(
        [ir, none],
        &expect![[r#"
            /roles/3/runtime: `helper` is a second agent, after `agent`: a workflow has exactly one
            /roles/0/runtime/agent/grants/2/now/payload_max_bytes: `resubmit`'s `payload_max_bytes` is 67108865, over the op payload limit of 67108864: give at most that
            /roles/0/runtime/agent/grants/3/job/target: `trainer` is the target of job functions `train` and `retrain`: name each target from one function
            /final/target: `trainer` is the final call's target and `train`'s: give the final call a role no job names
            /roles/2: `final_judge` is a target that no job function or final call names: name it from one
            /roles: no role is an agent: a workflow has exactly one"#]],
    );
}

/// The texts that a caller of `submit` gives its arguments, each `(name, text)`, and what
/// reading them against `submit`'s parameters gives.
struct Given(&'static [(&'static str, &'static str)]);

impl Case for Given {
    fn outcome(self) -> String {
        let function = ir::Name::from_static("submit");
        let parameters: BTreeMap<ir::Name, ir::Argument> = serde_json::from_value(json!({
            "finalize": { "type": "bool", "default": false },
            "seed": { "type": "int" },
            "note": { "type": "str", "default": "none" },
        }))
        .unwrap();
        let given = (self.0.iter())
            .map(|(name, text)| {
                let name = ir::Name::try_from((*name).to_owned()).unwrap();
                (name, (*text).to_owned())
            })
            .collect();
        match ir::argument::read(&function, &parameters, &given) {
            Ok(values) => format!("{values:?}"),
            Err(refused) => refused,
        }
    }
}

#[test]
fn caller_arguments() {
    check(
        [
            Given(&[("seed", "-3")]),
            Given(&[("seed", "+3"), ("finalize", "true"), ("note", "")]),
            Given(&[("seed", "3"), ("finalize", "yes")]),
            Given(&[("seed", "3.5")]),
            Given(&[]),
            Given(&[("seed", "3"), ("mode", "fast")]),
        ],
        &expect![[r#"
            {Name("finalize"): Bool(false), Name("note"): Str("none"), Name("seed"): Int(-3)}
            {Name("finalize"): Bool(true), Name("note"): Str(""), Name("seed"): Int(3)}
            `--arg finalize` is `"yes"`, which is no bool: give it `true` or `false`
            `--arg seed` is `"3.5"`, which `int` can't read: give it decimal digits, after `-` or `+` or neither, from -2^63 to 2^63 - 1
            `--arg seed` is required, as `submit` gives it no default: give it as `--arg seed=<value>`
            `--arg mode` is given, and `submit` takes no such argument: give only those that `ops` lists"#]],
    );
}
