//! The workflow IR, v0: the JSON form of a workflow.
//!
//! A workflow is data: [`Role`]s (one agent, its judges, with their [`Slot`]s, and the runners
//! they lease), the [`Grant`]s each role holds, and the [`Score`], which judges what the agent
//! submits.
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

pub(crate) mod values;

pub use values::{
    AbsolutePath, Argv, CpuFlag, CpuModel, Env, EnvVar, GpuModel, HeaderName, Image, JsonPointer,
    Mode, ModelName, Models, Name, Returns, RolePath, Sha256Digest, Shows, WorkflowName,
};

/// The IR's root: a workflow as written, which [`crate::workflow::Workflow::parse`] checks.
///
/// A run has two phases. In the work phase, the run starts the agent, which works until it
/// exits, finalizes a submission or reaches its deadline, calls the judges its grants name and
/// leases their runners. In the final phase, which starts once the agent's sandbox and every call
/// and lease it made, and every lease its calls made, are gone, the [`Score`] judge judges the
/// selection: the finalized submission, else the latest one stored. Only the run, which keeps the
/// submissions, lives through both phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub ir_version: IrVersion,
    pub name: WorkflowName,
    /// Exactly one of them has an agent runtime: the agent. The others are judges and runners.
    pub roles: Vec<Role>,
    /// How the selected submission is judged.
    pub score: Score,
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
    /// What each of the role's sandboxes needs of the class of sandbox it is created in, which
    /// placing the workflow finds among those that providers offer
    /// ([`Workflow::place`](crate::workflow::Workflow::place)).
    pub needs: Needs,
    pub runtime: Runtime,
    /// The role's routes out; absent, the role has none. A runner has none; the agent's model
    /// route is open while it works, a judge's while its command runs, in each of its calls.
    #[serde(default, skip_serializing_if = "Egress::is_empty")]
    pub egress: Egress,
    /// The ops the role may use while it works; absent, none. The agent holds judge calls,
    /// leases, and exactly one submit grant; a judge may hold leases, which each of its calls
    /// uses afresh, while its command runs; a runner holds none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
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
    /// A judge, whose command runs once per call in a fresh instance.
    Judge(JudgeRuntime),
    /// A runner, which runs the commands its lease's holder sends, in a fresh instance for each
    /// lease ([`LeaseGrant`]).
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
    /// The text the command reads on its standard input, which the runtime puts at
    /// [`command_stdin()`](crate::sandbox::command_stdin); absent, its standard input is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// How long the work phase may last.
    ///
    /// The clock starts once the agent's sandbox and its command have started, or, at the latest,
    /// once its class's [`startup_seconds`](crate::workflow::Class::startup_seconds) have passed
    /// since the run's start, so the time the sandbox takes to start, which that startup bounds
    /// instead, doesn't count against it.
    /// Judging the selected submission comes after the deadline, bounded instead by the
    /// score's [`timeout_seconds`](Score::timeout_seconds).
    pub deadline_seconds: NonZeroU64,
    /// How long the agent may keep running after its final submission is accepted. Its ops are
    /// refused meanwhile, while its [model route](ModelRoute) stays open, and the work phase,
    /// with the route, ends when it exits, when this runs out, or at the deadline, whichever
    /// comes first. 0 ends the work phase at the final submission.
    pub finalize_grace_seconds: u64,
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

/// What a judge call writes as its result: the JSON file at
/// [`result_file()`](crate::sandbox::result_file), of which its run shows the values
/// [`shows`](Self::shows) names, and its caller's reply returns the fields
/// [`returns`](Self::returns) names.
///
/// Once the judge's command exits, the result alone says how the call went, whatever the exit
/// status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSpec {
    /// The values of the result that the run shows, of any JSON type. A value the result lacks
    /// is left out; none is shown if this is absent.
    #[serde(default, skip_serializing_if = "Shows::is_empty")]
    pub shows: Shows,
    /// The fields of the result that the caller's reply returns. A field the result lacks is
    /// left out.
    pub returns: Returns,
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
    pub model: Option<Model>,
}

impl Egress {
    pub fn is_empty(&self) -> bool {
        let Self { model } = self;
        model.is_none()
    }
}

/// A role's model route, and the budget that bounds what the role spends through it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub route: ModelRoute,
    /// None for a route whose spend is unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ModelBudget>,
}

/// The most that a role's model requests may cost, over all its sandboxes and every model of its
/// route, at the prices that the run's configuration gives each model's tokens.
///
/// A run starts only if every kind of token of each model of the route has a price. What is
/// counted is the tokens that each response reported, priced once it has ended; one that reported
/// none, or more input read from and written to the cache than input, adds nothing. Once the
/// count reaches the budget, the route refuses each later request of the role with a 400 that
/// says that the role's model budget is exhausted. Requests in flight then are answered all the
/// same, so the spend may pass the budget by what they cost.
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

/// Access to the model provider: an entry for each API in which the role's code writes model
/// requests, one at least, and each API in one entry alone.
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
        if let Some(entry) = entries.iter().find(|entry| !seen.insert(entry.api)) {
            return Err(format!(
                "the API `{}` has more than one entry: give each API one, which lists every \
                 model its requests may name",
                entry.api
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
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Responses => "responses",
            Self::Chat => "chat",
            Self::Messages => "messages",
        })
    }
}

/// Permission for the role that holds it to use an op while it works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    Judge(JudgeGrant),
    Lease(LeaseGrant),
    Submit(SubmitGrant),
}

impl Grant {
    /// The name its holder uses the grant by. Unique among the role's grants.
    pub fn name(&self) -> &Name {
        match self {
            Self::Judge(JudgeGrant { name, .. })
            | Self::Lease(LeaseGrant { name, .. })
            | Self::Submit(SubmitGrant { name }) => name,
        }
    }
}

/// Runs a payload in a fresh instance of a judge role and collects its result.
///
/// The grant runs one call at a time: a call made while one of its calls is queued or running
/// is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeGrant {
    pub name: Name,
    /// The judge role, which no other judge grant names, so that the judge, like the grant,
    /// runs one call at a time.
    pub role: Name,
    /// The judge's slot that receives the payload.
    pub slot: Name,
    /// The most calls the agent may make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<NonZeroU64>,
    /// The most judge command time the agent's calls may use in all. A command's time counts
    /// against it once the command has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_seconds: Option<NonZeroU64>,
    /// The timeout of each call, counted from the start of the judge command. A call also
    /// ends with the grant's cumulative time, or with the work phase, if either comes first.
    pub call_timeout_seconds: NonZeroU64,
}

/// Leases runners of one role, each a fresh instance of the role, which runs the commands its
/// holder sends.
///
/// A lease runs one command at a time, until the holder closes it, its time runs out, or the
/// holder's work ends: the work phase, for the agent; its command, for a judge's call. Its time
/// runs from when its runner is ready until it ends. A judge's limits are each call's: a call
/// starts with all of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseGrant {
    pub name: Name,
    /// The runner role, which no other lease grant names. An agent's runner has an image that no
    /// judge has: a judge's files are in its image alone, so a runner that a judge trusts with
    /// them runs that image, and any other runner a judge leases may share the agent's runner's.
    pub role: Name,
    /// The most leases open at once, each until its runner is gone.
    pub instances: NonZeroU32,
    /// The most time each lease may last.
    pub instance_seconds: NonZeroU64,
    /// The most time the grant's leases may last in all, each lease open at once counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_seconds: Option<NonZeroU64>,
    /// The most commands the grant's leases may run in all, each transfer of a tree to or from
    /// a lease's runner counting as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<NonZeroU64>,
    /// The most bytes of files each transfer may move, at most
    /// [`MAX_TRANSFER_BYTES`](crate::sandbox::MAX_TRANSFER_BYTES): a tree that holds more is
    /// refused as it is collected.
    pub transfer_bytes: NonZeroU64,
}

/// Stores submissions, of which the [`Score`] judges the one selected. A submission may hold as
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
    /// The call's timeout, counted from the start of the judge command.
    pub timeout_seconds: NonZeroU64,
}
