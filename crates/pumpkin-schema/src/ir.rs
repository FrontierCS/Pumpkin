//! The workflow IR, v0: the JSON form of a workflow.
//!
//! A workflow is data: [`Role`]s (one agent and its judges, with their [`Slot`]s), and the
//! pattern, [`Main`], which gives the agent its [`Grant`]s and judges what it submits.
//!
//! The IR states every value. An optional field that is absent means what its doc says, such
//! as no limit or no route, never a default decided elsewhere; defaults belong to whatever
//! writes the IR.
//!
//! Deserialization checks each value on its own, so a deserialized IR holds only well-formed
//! names, images, paths and limits. [`crate::workflow::Workflow::parse`] checks how the parts
//! fit together.
//!
//! The crate's tests parse a complete workflow, the LWE task's, in `tests/fixtures/lwe.json`.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

pub(crate) mod values;

pub use values::{
    AbsolutePath, Argv, Env, EnvVar, Image, JsonPointer, ModelName, Models, Name, Returns,
    Sha256Digest, WorkflowName,
};

/// A workflow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub ir_version: IrVersion,
    pub name: WorkflowName,
    /// Exactly one of them has an agent runtime: the agent. The others are judges.
    pub roles: Vec<Role>,
    /// The pattern that drives a run.
    pub main: Main,
}

/// The IR's version. Each version serializes to its own string, so a consumer refuses an IR
/// whose version it does not know.
///
/// Deserialization reports the first error in the order the IR's fields are written, so the
/// refusal names the version only when `ir_version` comes first, as [`Workflow`] serializes
/// it. An IR from another version that writes a changed field earlier fails on that field
/// instead.
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
    /// The role's routes out; absent, the role has none. Only the agent may have one.
    #[serde(default, skip_serializing_if = "Egress::is_empty")]
    pub egress: Egress,
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
    /// Where inbound values land.
    pub slots: Vec<Slot>,
    /// What each call writes as its result.
    pub result: ResultSpec,
}

/// What a judge call writes as its result: the JSON file `/pumpkin/out/result.json`, which its
/// caller sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSpec {
    /// Points to the call's status in the result: the string `"passed"` or `"failed"`.
    pub status: JsonPointer,
    /// Points to the call's score in the result: a number, which a passed result must have and
    /// a failed one must not have, `null` counting as no score.
    pub score: JsonPointer,
    /// The fields of the result that the caller's reply returns, besides the status. A field
    /// the result lacks is left out.
    pub returns: Returns,
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
    /// The most bytes a payload into the slot may hold, at most
    /// [`MAX_OP_PAYLOAD_BYTES`](crate::sandbox::MAX_OP_PAYLOAD_BYTES), since every payload
    /// reaches its judge through an op. The [`Score`] slot's also caps every submission, so that
    /// whatever is selected fits it.
    pub max_bytes: NonZeroU64,
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

/// The pattern that drives a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Main {
    SoloThenJudge(SoloThenJudge),
}

/// One agent works until it exits, finalizes a submission or reaches the deadline; then the
/// pattern judges the selected submission with the [`Score`] judge.
///
/// A run has two phases. In the work phase, the agent works and calls the judges its grants
/// name. In the final phase, which starts once the agent's sandbox and every call it made are
/// gone, the score judge judges the selection: the finalized submission, else the latest one
/// stored. Only the pattern itself, which keeps the submissions, lives through both phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoloThenJudge {
    /// The ops the agent may use while it works: judge calls, and exactly one submit grant.
    pub grants: Vec<Grant>,
    /// How the selected submission is judged.
    pub score: Score,
    /// How long the work phase may last.
    ///
    /// The clock starts with the run, so starting the agent's sandbox counts against it.
    /// Judging the selected submission comes after the deadline, bounded instead by the
    /// score's [`timeout_seconds`](Score::timeout_seconds).
    pub deadline_seconds: NonZeroU64,
    /// How long the agent may keep running after its final submission is accepted, so that
    /// its harness can finish its last turn. Its ops are refused meanwhile, and the work phase
    /// ends when it exits, when this runs out, or at the deadline, whichever comes first. 0
    /// ends the work phase at the final submission.
    pub finalize_grace_seconds: u64,
}

/// Permission for the agent to use an op while it works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    Judge(JudgeGrant),
    Submit(SubmitGrant),
}

impl Grant {
    /// The name the agent uses the grant by. Unique among the agent's grants.
    pub fn name(&self) -> &Name {
        match self {
            Self::Judge(JudgeGrant { name, .. }) | Self::Submit(SubmitGrant { name }) => name,
        }
    }
}

/// Runs a payload in a fresh instance of a judge role and collects its result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeGrant {
    pub name: Name,
    /// The judge role.
    pub role: Name,
    /// The judge's slot that receives the payload.
    pub slot: Name,
    /// The most calls the agent may make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<NonZeroU64>,
    /// The most judge command time the agent's calls may use in all. A command's time counts
    /// against it once the command has ended, so calls that run at once may together overshoot
    /// it, by up to `concurrency` − 1 call timeouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_seconds: Option<NonZeroU64>,
    /// The most calls that may run at once. A call beyond it is refused.
    pub concurrency: NonZeroU32,
    /// The timeout of each call, counted from the start of the judge command. A call also
    /// ends with the grant's cumulative time, or with the work phase, if either comes first.
    pub call_timeout_seconds: NonZeroU64,
}

/// Stores submissions, of which the pattern judges the one selected. A submission may hold as
/// many bytes as the [`Score`] slot takes, its [`max_bytes`](Slot::max_bytes).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitGrant {
    pub name: Name,
}

/// The judge call that scores the selected submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Score {
    /// The judge role, which no [`JudgeGrant`] names, so that the agent never calls the judge
    /// that scores it.
    pub role: Name,
    /// The judge's slot that receives the submission.
    pub slot: Name,
    /// The call's timeout, counted from the start of the judge command, so that judging the
    /// selection always ends.
    pub timeout_seconds: NonZeroU64,
}
