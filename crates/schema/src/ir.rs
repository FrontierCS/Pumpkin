//! The workflow IR: the JSON form of a workflow.
//!
//! A workflow is data: [`Role`]s (the agents, the targets their calls start, and the runners that
//! sessions lease), the [`Grant`]s each role holds, and the [`Final`] call that judges the
//! submissions.
//!
//! The IR states every value. An optional field that is absent means what its doc says, such
//! as no limit or no route, never a default decided elsewhere; defaults belong to whatever
//! writes the IR.
//!
//! Deserialization checks each value on its own, so a deserialized IR holds only well-formed
//! names, images, paths, modes and limits. [`crate::workflow::Workflow::parse`] checks how the
//! parts fit together, and the limits a protocol bounds.

use std::collections::BTreeSet;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

pub mod argument;
mod final_call;
mod grant;
mod job;
mod label;
mod now;
mod session;
pub(crate) mod values;

pub use argument::{Argument, ArgumentType, ArgumentValue};
pub use final_call::{Final, FinalBody};
pub use grant::{Body, Function, Grant};
pub use job::JobFunction;
pub use label::{Label, Root};
pub use now::NowFunction;
pub use session::{NamedSession, Session};
pub use values::{
    AbsolutePath, Argv, CpuFlag, CpuModel, Env, EnvVar, GpuModel, HeaderName, Image, JsonPointer,
    Mode, ModelName, Models, Name, RolePath, Sha256Digest, WorkflowName,
};

/// The IR's root: a workflow as written, which [`crate::workflow::Workflow::parse`] checks.
///
/// During work each agent calls its grants, leases runners and may submit a payload. The
/// work deadline bounds all agents. Once their sandboxes, calls and leases are gone, the
/// [`Final`] call judges the submissions kept by the run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub ir_version: IrVersion,
    pub name: WorkflowName,
    /// How long all agents may work, counted from their start, at the latest from the longest
    /// agent startup bound after run start.
    pub deadline_seconds: NonZeroU64,
    /// The sandboxes in declaration order, including at least one agent.
    pub roles: Vec<Role>,
    /// How the submissions are judged.
    #[serde(rename = "final")]
    pub final_call: Final,
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
    #[serde(rename = "pumpkin.ir/v1")]
    V1,
}

/// A sandbox template. Every instance of a role is a sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub name: Name,
    pub image: Image,
    /// The user the role's commands run as.
    pub user: User,
    /// What each of the role's sandboxes needs of the class of sandbox it is created in, which
    /// placing the workflow finds among those that providers offer
    /// ([`Workflow::place`](crate::workflow::Workflow::place)).
    pub needs: Needs,
    pub runtime: Runtime,
}

/// A numeric user and group, neither of them root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub uid: NonZeroU32,
    pub gid: NonZeroU32,
}

/// What each instance of a role needs of its sandbox: its CPU and memory, and GPUs, a CPU's flags
/// and a CPU's model, each of which, absent, it doesn't need.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Needs {
    /// The CPU limit, in thousandths of a CPU.
    pub cpu_millis: NonZeroU64,
    /// The memory limit.
    pub memory_bytes: NonZeroU64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<Gpu>,
    /// The flags of the CPU that the role's commands rely on, such as `avx512f`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub cpu_features: BTreeSet<CpuFlag>,
    /// The model of the CPU that the role's sandboxes run on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<CpuModel>,
}

impl Needs {
    /// The limits that a provider gives each of the role's sandboxes.
    pub fn resources(&self) -> Resources {
        Resources {
            cpu_millis: self.cpu_millis,
            memory_bytes: self.memory_bytes,
        }
    }
}

/// GPUs of one model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gpu {
    pub count: NonZeroU32,
    pub model: GpuModel,
}

/// The CPU and memory limits of each sandbox of a role ([`Needs::resources`]), as a provider
/// gives them, and as the agent's `ops` reply says them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// The CPU limit, in thousandths of a CPU.
    pub cpu_millis: NonZeroU64,
    pub memory_bytes: NonZeroU64,
}

/// The thousandths of a CPU in a CPU, the unit of a CPU limit ([`Resources::cpu_millis`]).
pub const MILLIS_PER_CPU: u64 = 1000;

/// What runs in a role's sandboxes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// An agent harness.
    Agent(AgentRuntime),
    /// A target runs the commands a call's body sends.
    Target(TargetRuntime),
    /// A runner, which runs the commands its lease's holder sends, in a fresh instance for each
    /// lease ([`Session`]).
    Runner(RunnerRuntime),
}

/// An agent harness: its directories and files, which the runtime puts in the agent's sandbox
/// before its command starts, then the command, which reads its standard input's text.
///
/// The runtime makes each of them as the role's user, with its mode as a tree gives it, `dirs`
/// first, in order of their paths, so a directory comes before what it holds: each entry's
/// parent is in the image or in `dirs`. A file the image has already fails the agent's start, and
/// a directory it has already is kept as it is. No path is put twice, and none is below a file.
///
/// The agent can read `files`, `stdin` and `env`, and the IR is recorded whole: keep secrets out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRuntime {
    /// The command that launches the harness.
    pub command: Command,
    /// Absent, none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<Dir>,
    /// Absent, none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<File>,
    /// Prepend this role's grant guide and a task heading to `stdin`. Absent or false, pass
    /// `stdin` verbatim.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepend_grant_help: bool,
    /// The task input, empty when absent. The runtime puts the resulting command input at
    /// [`command_stdin()`](crate::sandbox::command_stdin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// How long the agent may keep running after a call of its ends its work. Its ops are
    /// refused meanwhile, while its [model route](ModelRoute) stays open. The agent ends on exit,
    /// at the end of its grace or at the shared deadline, whichever comes first. 0 ends its work
    /// in that call's step.
    pub finalize_grace_seconds: u64,
    /// The agent's routes out, open while it works; absent, it has none.
    #[serde(default, skip_serializing_if = "Egress::is_empty")]
    pub egress: Egress,
    /// The functions the agent may call while it works.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
}

/// A directory to make in the agent's sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dir {
    pub path: RolePath,
    pub mode: Mode,
}

/// A text file to write in the agent's sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: RolePath,
    pub mode: Mode,
    pub content: String,
}

/// How a runner runs each command its lease's holder sends, as the role's user: as a
/// [`Command`] would with the holder's argument vector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerRuntime {
    /// The directory each command starts in.
    pub cwd: AbsolutePath,
    /// Variables added to each command's environment, as [`Command::env`]'s are.
    #[serde(default, skip_serializing_if = "Env::is_empty")]
    pub env: Env,
}

/// The lifetime of a target sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetLifetime {
    /// Each call owns its sandbox until its body ends.
    PerCall,
    /// All work-phase calls use one sandbox, one complete body at a time, which keeps the files
    /// each body leaves, a failed one's too. A call while another's body runs, or while that
    /// body's runner is being deleted, is refused as busy, and isn't charged. Once the sandbox's
    /// session has ended, each later call is refused, and the agents work on. Once the work phase's
    /// calls have ended, the run collects the sandbox's output and deletes it, before the final
    /// call.
    WorkPhase,
}

/// How a target runs each command a call's body sends, as the role's user: as a [`Command`] would
/// with the body's argument vector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetRuntime {
    /// Which calls share a sandbox. Absent, each call has its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<TargetLifetime>,
    /// The directory each command starts in.
    pub cwd: AbsolutePath,
    /// Variables added to each command's environment, as [`Command::env`]'s are.
    #[serde(default, skip_serializing_if = "Env::is_empty")]
    pub env: Env,
    /// The target's routes out, open while a call's body runs; absent, it has none.
    #[serde(default, skip_serializing_if = "Egress::is_empty")]
    pub egress: Egress,
    /// The body's session grants, with fresh quotas per call. A `per_call` sandbox's
    /// commands may also use them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<NamedSession>,
}

/// A process to start in a sandbox, as the role's user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// The whole command line; the image's own entrypoint and default command are not used.
    pub argv: Argv,
    /// Variables added to the image's environment. The image's variables come first, then
    /// [Pumpkin's own](crate::sandbox::command_env), then these, each overriding the ones
    /// before.
    #[serde(default, skip_serializing_if = "Env::is_empty")]
    pub env: Env,
    /// The directory the command starts in.
    pub cwd: AbsolutePath,
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

/// The most that a role's model requests through one entry of its route may cost, over all the
/// role's sandboxes, at the prices that the run's configuration gives each model's tokens.
///
/// A run starts only if every kind of token of each model of the entry has a price. What is
/// counted is the tokens that each response through the entry reported, priced once it has ended;
/// one that reported none, or more input read from and written to the cache than input, adds
/// nothing. Once the count reaches the budget, the route refuses each later request through the
/// entry with a 400 that says that its budget is exhausted, and no other entry's. Requests in
/// flight then are answered all the same, so the spend may pass the budget by what they cost.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBudget {
    pub microusd: NonZeroU64,
}

impl ModelBudget {
    /// Whether a spend of `usd`, in US dollars, has reached the budget.
    pub fn spent_by(&self, usd: f64) -> bool {
        // Reached at the budget itself, as `LiteLLM`'s proxy refuses a key once
        // `spend >= max_budget`:
        // https://github.com/BerriAI/litellm/blob/df78b16941697991a736fc224182fa17270420ed/litellm/proxy/auth/auth_checks.py#L6766-L6787
        usd >= self.usd()
    }

    /// `usd`, a sum in US dollars, to the cent at least and to as many more places as the budget
    /// states: `$20.04` beside a budget of `$20.00`.
    pub fn dollars(&self, usd: f64) -> String {
        let mut millionths = self.microusd.get() % 1_000_000;
        let mut places = 6;
        while places > 2 && millionths.is_multiple_of(10) {
            millionths /= 10;
            places -= 1;
        }
        format!("${usd:.places$}")
    }

    fn usd(&self) -> f64 {
        #[expect(
            clippy::cast_precision_loss,
            reason = "exact below 2^53 millionths of a dollar, past any budget's need"
        )]
        let microusd = self.microusd.get() as f64;
        microusd / 1e6
    }
}

/// In US dollars, as [`ModelBudget::dollars`] gives them: `$20.00`, `$0.0005`.
impl fmt::Display for ModelBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dollars(self.usd()))
    }
}

/// Access to the model provider: one entry at least, each for an API in which the role's code
/// writes model requests, and each model in one entry alone for its API.
///
/// A request goes through the entry for its API that allows its model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ApiRoute>", into = "Vec<ApiRoute>")]
pub struct ModelRoute(Vec<ApiRoute>);

impl ModelRoute {
    /// Its entries, in the IR's order.
    pub fn as_slice(&self) -> &[ApiRoute] {
        &self.0
    }
}

impl TryFrom<Vec<ApiRoute>> for ModelRoute {
    type Error = String;

    fn try_from(entries: Vec<ApiRoute>) -> Result<Self, Self::Error> {
        if entries.is_empty() {
            return Err("no APIs: a model route has an entry for one at least".to_owned());
        }
        let mut seen = BTreeSet::new();
        let mut models = (entries.iter()).flat_map(|entry| {
            (entry.models.as_slice().iter()).map(move |model| (entry.api, model))
        });
        if let Some((api, model)) = models.find(|named| !seen.insert(*named)) {
            return Err(format!(
                "`{model}` is in more than one `{api}` entry: list each model in one entry for \
                 its API"
            ));
        }
        Ok(Self(entries))
    }
}

impl From<ModelRoute> for Vec<ApiRoute> {
    fn from(route: ModelRoute) -> Self {
        route.0
    }
}

/// A model route's entry for one API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiRoute {
    /// The API that the requests are written in.
    pub api: Api,
    /// The models that requests may name.
    pub models: Models,
    /// The request headers that the route passes upstream as sent, besides the API's own: those
    /// a client's product adds to it. Any other request header stays behind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub request_headers: Vec<HeaderName>,
    /// The response headers that the route passes back as the upstream sent them, besides the
    /// API's own. Any other response header stays behind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub response_headers: Vec<HeaderName>,
    /// None for an entry whose spend is unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ModelBudget>,
}

/// An API of model providers', in which a role's code writes its model requests. It shows as
/// the IR names it.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// OpenAI's Responses API.
    Responses,
    /// OpenAI's Chat Completions API, which most clients of a provider that speaks OpenAI's API
    /// write.
    Chat,
    /// Anthropic's Messages API.
    Messages,
    /// Google's Gemini API: its `generateContent` and `streamGenerateContent` methods.
    GenerateContent,
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
            Self::Messages => "messages",
            Self::GenerateContent => "generate_content",
        })
    }
}
