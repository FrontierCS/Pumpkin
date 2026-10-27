//! The workflow IR, v0: the JSON form of a workflow.
//!
//! A workflow is data, and each part is a type here:
//!
//! - A [`Role`] is a sandbox template; every instance of a role is a sandbox. A role is an
//!   agent, which runs a harness and is untrusted, or a judge, whose command runs once per
//!   call in a fresh instance.
//! - A [`Slot`] is a place in a judge where an inbound value lands. A value in a code slot
//!   gives whoever influenced it control of the judge; a value in a data slot is parsed, never
//!   executed, so it carries information but not control.
//! - A judge's [`Mode`] tells judges of work in progress (train) from judges of the final
//!   submission (final).
//! - A [`Grant`] lets a principal (a role, or the root, which is the workflow's pattern) use
//!   an [`Op`] in some phases.
//! - A [`Phase`] is a stage of a run. Entering a phase destroys the instances of the roles
//!   not live in it.
//! - [`Main`] is the pattern that drives the run.
//!
//! The IR states every value. An optional field that is absent means what its doc says, such
//! as no limit or no route, never a default decided elsewhere; defaults belong to whatever
//! writes the IR.
//!
//! Deserialization checks each value on its own, so a deserialized IR holds only well-formed
//! names, images, paths and limits. [`crate::workflow::Workflow::parse`] checks how the parts
//! fit together.
//!
//! # Example
//!
//! One agent submits, and the root scores the submission with a final-mode judge:
//!
//! ```
//! use pumpkin_schema::{ir, workflow::Workflow};
//!
//! let ir: ir::Workflow = serde_json::from_str(
//!     r#"{
//!   "ir_version": "pumpkin.ir/v0",
//!   "name": "example/solo",
//!   "roles": [
//!     {
//!       "name": "agent",
//!       "image": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
//!       "user": { "uid": 1000, "gid": 1000 },
//!       "resources": {
//!         "cpu_millis": 2000,
//!         "memory_bytes": 4294967296,
//!         "workspace_bytes": 4294967296
//!       },
//!       "runtime": {
//!         "agent": { "command": { "argv": ["/usr/local/bin/agent"], "cwd": "/workspace" } }
//!       }
//!     },
//!     {
//!       "name": "judge",
//!       "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
//!       "user": { "uid": 1000, "gid": 1000 },
//!       "resources": {
//!         "cpu_millis": 2000,
//!         "memory_bytes": 4294967296,
//!         "workspace_bytes": 1073741824
//!       },
//!       "runtime": {
//!         "judge": {
//!           "command": { "argv": ["/usr/local/bin/judge"], "cwd": "/pumpkin/home" },
//!           "mode": "final",
//!           "trust": "trusted",
//!           "slots": [{ "name": "payload", "kind": "data", "max_bytes": 1048576 }]
//!         }
//!       }
//!     }
//!   ],
//!   "grants": [
//!     {
//!       "name": "submit",
//!       "holder": { "role": "agent" },
//!       "phases": ["work"],
//!       "op": { "submit": { "max_bytes": 1048576 } }
//!     },
//!     {
//!       "name": "score",
//!       "holder": "root",
//!       "phases": ["final"],
//!       "op": { "judge": { "role": "judge", "slot": "payload", "concurrency": 1 } }
//!     }
//!   ],
//!   "phases": [
//!     { "name": "work", "live": ["agent"], "next": ["final"] },
//!     { "name": "final", "live": ["judge"] }
//!   ],
//!   "main": {
//!     "solo_then_judge": {
//!       "agent": "agent",
//!       "submit": "submit",
//!       "score": "score",
//!       "work_phase": "work",
//!       "final_phase": "final",
//!       "deadline_seconds": 3600
//!     }
//!   }
//! }"#,
//! )?;
//! let workflow = Workflow::parse(&ir)?;
//! assert_eq!(workflow.name().as_str(), "example/solo");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

mod values;

pub use values::{
    AbsolutePath, Argv, Env, EnvVar, Image, ModelName, Models, Name, Sha256Digest, WorkflowName,
};

/// A workflow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub ir_version: IrVersion,
    pub name: WorkflowName,
    pub roles: Vec<Role>,
    pub grants: Vec<Grant>,
    /// The phases a run moves through. A run starts in the first one.
    pub phases: Vec<Phase>,
    /// The pattern that drives a run.
    pub main: Main,
}

/// The IR's version. Each version serializes to its own string, so a consumer refuses an IR
/// whose version it does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrVersion {
    #[serde(rename = "pumpkin.ir/v0")]
    V0,
}

/// A sandbox template. Every instance of a role is a sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub name: Name,
    pub image: Image,
    /// The user the role's commands run as.
    pub user: User,
    pub resources: Resources,
    pub runtime: Runtime,
    #[serde(default, skip_serializing_if = "Egress::is_empty")]
    pub egress: Egress,
    /// Data the role's image holds, declared for the per-mode rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assets: Vec<Asset>,
}

/// A numeric user and group, neither of them root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub uid: NonZeroU32,
    pub gid: NonZeroU32,
}

/// The resources of each instance of a role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// The CPU limit, in thousandths of a CPU.
    pub cpu_millis: NonZeroU64,
    pub memory_bytes: NonZeroU64,
    /// The size limit of the role's writable storage.
    pub workspace_bytes: NonZeroU64,
}

/// Whether the workflow's author vouches for a role's own code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// The role's code acts as its author intends, though code it is given may not.
    Trusted,
    /// The role's code may act against the run. Every agent is untrusted.
    Untrusted,
}

/// The mode of a judge. Final-mode data, such as hidden evaluation data, may only be held by
/// final-mode roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Judges work in progress; its results may reach the agent.
    Train,
    /// Judges the final submission.
    Final,
}

/// What runs in a role's sandboxes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// An agent harness. Agents are untrusted, and whatever reaches them is code.
    Agent(AgentRuntime),
    /// A judge, whose command runs once per call in a fresh instance.
    Judge(JudgeRuntime),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRuntime {
    /// The command that launches the harness.
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeRuntime {
    /// The command each judge call runs.
    pub command: Command,
    pub mode: Mode,
    /// Declared for every judge, so that no trust decision is implicit.
    pub trust: Trust,
    /// Where inbound values land.
    pub slots: Vec<Slot>,
}

/// A process to start in a sandbox, as the role's user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// The whole command line; the image's own entrypoint and default command are not used.
    pub argv: Argv,
    /// Variables added to the image's environment. The image's variables come first, then
    /// Pumpkin's own, then these, each overriding the ones before.
    #[serde(default, skip_serializing_if = "Env::is_empty")]
    pub env: Env,
    /// The directory the command starts in.
    pub cwd: AbsolutePath,
}

/// A named place where an inbound value lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub name: Name,
    pub kind: SlotKind,
    pub max_bytes: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    /// Run by the receiver: whoever influenced the value controls the receiver.
    Code,
    /// Parsed, never executed: the value carries information but not control.
    Data,
}

/// The routes out of a sandbox beyond its own session, all served by the gateway.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRoute>,
}

impl Egress {
    pub fn is_empty(&self) -> bool {
        let Self { model } = self;
        model.is_none()
    }
}

/// Access to the model provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoute {
    /// The models that requests may name.
    pub models: Models,
    /// The cap on the role's model spending over a run, in millionths of a US dollar.
    pub budget_microusd: NonZeroU64,
}

/// Data a role's image holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub name: Name,
    /// Where the asset is in the role's image.
    pub path: AbsolutePath,
    /// The digest of the asset's content.
    pub digest: Sha256Digest,
    pub mode: AssetMode,
}

/// Which roles may hold an asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetMode {
    /// Any role, whatever its mode: public datasets, model weights.
    Any,
    /// Final-mode roles only: hidden evaluation data, such as final workloads, seeds and
    /// graded targets.
    Final,
}

/// Permission for a principal to use an op in some phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// The name the holder uses the grant by. Unique within the workflow.
    pub name: Name,
    pub holder: Principal,
    /// The phases in which the grant applies.
    pub phases: Vec<Name>,
    pub op: Op,
}

/// The holder of a grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Principal {
    /// The workflow's pattern.
    Root,
    /// Every instance of the named role.
    Role(Name),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Judge(JudgeOp),
    Submit(SubmitOp),
}

/// Runs a payload in a fresh instance of a judge role and collects its result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeOp {
    /// The judge role.
    pub role: Name,
    /// The judge's slot that receives the payload.
    pub slot: Name,
    /// The most calls the holder may make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<NonZeroU64>,
    /// The most judge command time the holder's calls may use in all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_seconds: Option<NonZeroU64>,
    /// The most calls that may run at once. A call beyond it is refused.
    pub concurrency: NonZeroU32,
    /// The timeout of each call, counted from the start of the judge command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_timeout_seconds: Option<NonZeroU64>,
}

/// Stores submissions, of which the pattern judges the one selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitOp {
    pub max_bytes: NonZeroU64,
}

/// A phase of a run. Entering a phase destroys the instances of the roles not live in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub name: Name,
    /// The roles whose instances may exist in this phase.
    pub live: Vec<Name>,
    /// The phases this phase may be followed by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next: Vec<Name>,
}

/// The pattern that drives a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Main {
    SoloThenJudge(SoloThenJudge),
}

/// One agent works until it exits, finalizes a submission or reaches the deadline; then the
/// pattern enters the final phase and judges the selected submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoloThenJudge {
    /// The agent role.
    pub agent: Name,
    /// The agent's submit grant.
    pub submit: Name,
    /// The root's judge grant that scores the selected submission.
    pub score: Name,
    /// The initial phase, in which the agent works.
    pub work_phase: Name,
    /// The phase in which the submission is judged.
    pub final_phase: Name,
    /// How long the work phase may last.
    ///
    /// The clock starts with the run, so starting the agent's sandbox counts against it.
    /// Judging the selected submission comes after the deadline, bounded instead by the score
    /// grant's [`JudgeOp::call_timeout_seconds`].
    pub deadline_seconds: NonZeroU64,
}
