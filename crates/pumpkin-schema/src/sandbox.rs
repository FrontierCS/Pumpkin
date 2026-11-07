//! The sandbox protocol, v0: what the gateway and `sandboxd` say to each other over a session.
//!
//! A session is one connection, which `sandboxd` opens to the gateway when its sandbox starts
//! and which lasts as long as the sandbox. The connection identifies the sandbox: the gateway
//! gives each sandbox a socket of its own, which takes a single connection.
//!
//! Both sides send [`frame`]s, each a message and a body of raw bytes: [`ToSandbox`] from the
//! gateway, [`FromSandbox`] from `sandboxd`. Every request from the gateway names a stream, a
//! number it uses once, and everything sent in reply names the same stream, so that replies
//! to several requests can interleave.
//!
//! Files move as trees: [`Entry`]s, each a directory or a regular file whose bytes follow in
//! `Data` frames, every entry after its parent's, then an `End`. A tree holds nothing else, so
//! no link or special file crosses a session, and every path in it is a [`RelativePath`],
//! which can't leave the directory the tree is written to; [`tree::Writer`] writes one there. A
//! tree the gateway puts gives each entry its [`Mode`].
//!
//! Ops go the other way: a caller in the sandbox asks the session's owner for something, such
//! as judging a payload ([`op`]). An op opens with [`FromSandbox::Op`], numbered apart from the
//! gateway's streams, and gives its payload's size first, so the gateway checks the size before
//! any byte arrives; the bytes follow in [`FromSandbox::OpData`], and the gateway answers once,
//! with [`ToSandbox::OpReply`]. A sandbox has at most one op in flight, from its `Op` to its
//! reply, so its callers take turns, and the owner answers each promptly: an op that starts
//! long work, as a judge call does, with a job that later ops ask about. A session takes ops
//! only if the gateway accepts it with an [`OpPayloadLimit`], and model requests
//! ([`FromSandbox::ModelRequest`]) only if it accepts it for them.
//!
//! Every provider presents [`pumpkin_dir()`], with [`pumpkin_subdirs()`] below it, in the
//! sandboxes it starts: the session's owner and a role's commands rely on them in any sandbox.
//! It holds at most [`PUMPKIN_BYTES`]. A sandbox whose role makes ops also has `sandboxctl` at
//! [`SANDBOXCTL_PATH`], and its `sandboxd` serves ops at [`CONTROL_SOCKET`]. Any other sandbox
//! is passive: nothing in it can make an op, and its session takes none. A sandbox whose role
//! has a model route has its `sandboxd` serve the model endpoint at [`MODEL_PORT`]. An agent's
//! image holds its harness below [`harness_dir()`]. A workflow puts nothing in these directories
//! ([`RolePath`](crate::ir::RolePath)).

use op::{OpCall, OpReply};
use serde::{Deserialize, Serialize};

pub use crate::ir::values::RelativePath;
use crate::ir::{AbsolutePath, Argv, Env, EnvVar, Mode, Name};

pub mod frame;
pub mod op;
pub mod tree;

/// The most bytes an op's payload may hold. `sandboxd` refuses a longer payload before it
/// opens the op, and no session's [`OpPayloadLimit`] is higher.
pub const MAX_OP_PAYLOAD_BYTES: u64 = 64 << 20;

/// The most bytes an op's payload may hold in a session that takes ops, which the gateway
/// accepts it with: at most [`MAX_OP_PAYLOAD_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpPayloadLimit(u64);

impl OpPayloadLimit {
    pub fn bytes(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for OpPayloadLimit {
    type Error = String;

    fn try_from(bytes: u64) -> Result<Self, Self::Error> {
        if bytes > MAX_OP_PAYLOAD_BYTES {
            return Err(format!(
                "the op payload limit is {bytes} bytes, over the protocol's \
                 {MAX_OP_PAYLOAD_BYTES}: give at most that"
            ));
        }
        Ok(Self(bytes))
    }
}

/// The most model requests a sandbox may have in flight: one for each agent of a harness that
/// runs a lead agent and up to six helpers at once, each waiting on one response at a time.
pub const MAX_MODEL_REQUESTS_IN_FLIGHT: usize = 7;

/// The most bytes a model request's body may hold: as many as an op's payload, since the gateway
/// holds each whole, as it holds a payload.
pub const MAX_MODEL_REQUEST_BYTES: u64 = MAX_OP_PAYLOAD_BYTES;

/// The directory that holds Pumpkin's files in every sandbox, `/pumpkin`, and belongs to the
/// role's user.
///
/// The session's owner puts a role's inputs there and collects its results from there; a
/// role's commands read and write them at the paths below it.
pub fn pumpkin_dir() -> AbsolutePath {
    valid("/pumpkin")
}

/// The directories below [`pumpkin_dir()`], there before a role's first command starts: the
/// in-sandbox contract that a task's images are written against, the same for every workflow.
///
/// - `in` holds what the session's owner puts in, as a judge call's payload
///   ([`slot_payload()`]) or the agent's standard input ([`command_stdin()`]);
/// - [`out_dir()`] what it collects, as a judge's result ([`result_file()`]) and evidence
///   ([`evidence_dir()`]);
/// - `home` the commands' home ([`command_env()`]);
/// - [`log_dir()`] their output ([`command_stdout()`], [`command_stderr()`]).
///
/// The paths these functions give are relative to [`pumpkin_dir()`], which
/// [`AbsolutePath::join`] joins them to. What the session's owner puts there has [`dir_mode()`]
/// or [`file_mode()`].
pub fn pumpkin_subdirs() -> [RelativePath; 4] {
    [in_dir(), out_dir(), home_dir(), log_dir()]
}

/// `0755`, the mode of each directory the session's owner puts below [`pumpkin_dir()`].
pub fn dir_mode() -> Mode {
    valid("0755")
}

/// `0644`, the mode of each file the session's owner puts below [`pumpkin_dir()`].
pub fn file_mode() -> Mode {
    valid("0644")
}

/// `in`, which the session's owner puts a role's inputs in.
fn in_dir() -> RelativePath {
    valid("in")
}

/// `home`, the home of a role's commands.
fn home_dir() -> RelativePath {
    valid("home")
}

/// `out`, which the session's owner collects a role's results from.
pub fn out_dir() -> RelativePath {
    valid("out")
}

/// `log`, which holds the output of a role's commands.
pub fn log_dir() -> RelativePath {
    valid("log")
}

/// `in/<slot>`, the directory of a judge's slot named `slot`.
pub fn slot_dir(slot: &Name) -> RelativePath {
    valid(&format!("{}/{slot}", in_dir()))
}

/// `in/<slot>/raw`, where a judge call's payload is, in the slot the call names, before the
/// judge's command starts.
pub fn slot_payload(slot: &Name) -> RelativePath {
    valid(&format!("{}/raw", slot_dir(slot)))
}

/// `out/result.json`, where a judge writes its result, of at most [`RESULT_MAX_BYTES`]: its
/// [`ResultSpec`](crate::ir::ResultSpec) says how it is read, and what of it the caller sees.
pub fn result_file() -> RelativePath {
    valid(&format!("{}/result.json", out_dir()))
}

/// The most bytes a judge's result ([`result_file()`]) may hold: a larger one is no result,
/// rejected whole rather than cut short. A judge keeps its bulk output in [`evidence_dir()`].
// The session's owner collects a result as a file in the sandbox's `out`, then reads it whole
// into memory to parse it, and a reply returns some of its fields: the bound keeps a result a
// small document, and what the host holds for it small. A reply has its own bound,
// `frame::MAX_MESSAGE_BYTES`. Cutting a larger result short would not help, since a JSON document
// cut short is no document. 1 MiB holds a status, a score and the fields a reply returns with
// room to spare.
pub const RESULT_MAX_BYTES: u64 = 1 << 20;

/// `out/evidence`, there before a judge's command starts, which holds the files of evidence
/// that only the operator sees.
pub fn evidence_dir() -> RelativePath {
    valid(&format!("{}/evidence", out_dir()))
}

/// The environment that a role's command, whose IR gives it `env`, runs with: `HOME` first, set
/// to `home` below [`pumpkin_dir()`], then `env`; or `env` alone, if it sets `HOME` itself.
pub fn command_env(env: &Env) -> Env {
    let home = pumpkin_dir().join(&home_dir());
    env.with_default(valid::<EnvVar>(&format!("HOME={home}")))
}

/// `log/command.stdout`, where a role's commands write their standard output.
pub fn command_stdout() -> RelativePath {
    valid(&format!("{}/command.stdout", log_dir()))
}

/// `log/command.stderr`, where a role's commands write their standard error.
pub fn command_stderr() -> RelativePath {
    valid(&format!("{}/command.stderr", log_dir()))
}

/// `in/command.stdin`, which the agent's command reads its standard input from, if its runtime
/// gives one ([`AgentRuntime::stdin`](crate::ir::AgentRuntime::stdin)).
pub fn command_stdin() -> RelativePath {
    valid(&format!("{}/command.stdin", in_dir()))
}

/// The most [`pumpkin_dir()`] holds, in bytes, so that whatever the session's owner collects
/// from below it fits in this many.
///
/// A provider may back it with the role's memory, so that its files and the role's processes
/// share the role's memory limit: a role can keep there at most the smaller of the two.
pub const PUMPKIN_BYTES: u64 = 1 << 30;

/// Where `sandboxctl` is in a sandbox whose role makes ops, `/.pumpkin/bin/sandboxctl`.
///
/// Every provider presents `sandboxd`'s binary there, for the role's user to execute, under the
/// file name that makes it run as `sandboxctl`. [`op`] gives what its commands print.
pub const SANDBOXCTL_PATH: &str = "/.pumpkin/bin/sandboxctl";

/// The control socket, where `sandboxd` serves ops in a sandbox whose role makes ops,
/// `/pumpkin/sandboxctl/socket`.
///
/// Every provider starts that sandbox's `sandboxd` with `--ops`, which makes it serve them
/// there, below [`pumpkin_dir()`], and `sandboxctl` connects there.
pub const CONTROL_SOCKET: &str = "/pumpkin/sandboxctl/socket";

/// The directory that holds an agent's harness, `/harness`, which a task's images leave out.
///
/// Pumpkin adds a harness to a task's agent image in a directory of its own below this one,
/// named for the harness and its version, such as `codex-0.167.0`.
pub fn harness_dir() -> AbsolutePath {
    valid("/harness")
}

/// The directories of a sandbox that hold Pumpkin's own files: [`pumpkin_dir()`]; `/.pumpkin`,
/// where providers present `sandboxd`, and `sandboxctl` at [`SANDBOXCTL_PATH`]; and
/// [`harness_dir()`].
pub(crate) fn pumpkin_dirs() -> [AbsolutePath; 3] {
    [pumpkin_dir(), valid("/.pumpkin"), harness_dir()]
}

/// The port of the model endpoint, on `127.0.0.1` in a sandbox whose role has a model route.
///
/// Its harness makes model requests ([`FromSandbox::ModelRequest`]) at
/// `http://127.0.0.1:<MODEL_PORT>`: by that address, not `localhost`, which may also name the
/// IPv6 loopback, where nothing listens. Every provider starts that sandbox's `sandboxd` with
/// `--model`, which makes it serve the endpoint there, and the session's owner accepts the
/// session to take model requests.
pub const MODEL_PORT: u16 = 11235;

/// `value`, one of this module's constant paths, variables or modes, or a path that holds a
/// slot's name, as the type it is.
fn valid<T: TryFrom<String, Error = String>>(value: &str) -> T {
    match T::try_from(value.to_owned()) {
        Ok(value) => value,
        #[expect(
            clippy::unreachable,
            reason = "each of this module's paths is normalized: `/pumpkin`, `/.pumpkin` and \
                      `/harness` are absolute, and each of `/pumpkin`'s directories and files, \
                      and a slot's `Name`, from a-z, 0-9, `_` and `-`, one plain component; \
                      `HOME` is a variable's name; each mode is `0` and three octal digits; \
                      checking them as the crate builds would take a `const fn` copy of each \
                      check, and a constructor that skips the checks"
        )]
        Err(error) => unreachable!("{error}"),
    }
}

/// What the gateway sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToSandbox {
    /// Writes a tree into `dir`, creating `dir` and its missing parents. The tree's entries
    /// follow on the same stream, and `sandboxd` replies once: when the tree has been written,
    /// or at the first failure.
    Put { stream: u32, dir: AbsolutePath },
    /// An entry of the tree being put, made with `mode` exactly, whatever `sandboxd`'s umask: a
    /// directory that exists already is kept as it is.
    Entry {
        stream: u32,
        entry: Entry,
        mode: Mode,
    },
    /// Bytes of the file being put, in the frame's body.
    Data { stream: u32 },
    /// The end of the tree being put.
    End { stream: u32 },
    /// Starts a command in a process group of its own. `sandboxd` replies when it has
    /// started, and later sends [`FromSandbox::Exited`] on the same stream.
    Start { stream: u32, launch: Launch },
    /// Kills every process in the sandbox but `sandboxd` and refuses to start more, so that
    /// what is collected afterwards can't change. `sandboxd` replies once they are gone.
    Freeze { stream: u32 },
    /// Sends the tree in `dir`: its entries, then an `End`.
    Collect { stream: u32, dir: AbsolutePath },
    /// The answer to op `op`, which ends it.
    OpReply { op: u32, reply: OpReply },
    /// The status and headers of the response to model request `request`, sent once the
    /// request is whole. Its body follows in `ModelData` frames, then a `ModelEnd`.
    ModelResponse {
        request: u32,
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// Bytes of the body of the response to model request `request`, in the frame's body.
    ModelData { request: u32 },
    /// The end of the response to model request `request`, which takes the request out of
    /// flight: whole, or `cut` short, which its client must not take as whole. With no
    /// `ModelResponse` before it, the request ends without a response, cut short.
    ModelEnd { request: u32, cut: bool },
}

/// What `sandboxd` sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FromSandbox {
    /// The request on `stream` succeeded.
    Done { stream: u32 },
    /// The request on `stream` failed, and nothing more follows on it.
    Failed { stream: u32, message: String },
    /// The command started on `stream` exited. Other processes in its group may live on.
    Exited { stream: u32, status: ExitStatus },
    /// An entry of the tree being collected.
    Entry { stream: u32, entry: Entry },
    /// Bytes of the file being collected, in the frame's body.
    Data { stream: u32 },
    /// The end of the tree being collected.
    End { stream: u32 },
    /// Opens op `op`, while no other op is in flight. Its payload, of `size` bytes, at most
    /// [`MAX_OP_PAYLOAD_BYTES`], follows in `OpData` frames. Only an [`OpCall::Judge`] or an
    /// [`OpCall::Submit`] takes one; `size` is 0 for any other call. The op is whole once
    /// `size` bytes of `OpData` have arrived, at once for 0, and the gateway answers it after
    /// that.
    Op { op: u32, call: OpCall, size: u64 },
    /// Bytes of op `op`'s payload, in the frame's body, which go no further than its size. An
    /// empty frame changes nothing.
    OpData { op: u32 },
    /// Opens model request `request` for a client of the model endpoint that `sandboxd` serves
    /// for a role with a model route: an HTTP request with the method, path and headers the
    /// client sent, the headers' names in lowercase. Its number is none that a model request in
    /// flight has, in a number space apart from ops', and it is in flight from here to its
    /// response's [`ToSandbox::ModelEnd`]: at most [`MAX_MODEL_REQUESTS_IN_FLIGHT`] are. Its
    /// body, of `size` bytes, at most [`MAX_MODEL_REQUEST_BYTES`], follows in `ModelData`
    /// frames, as an op's payload does, and the gateway responds once all of it has arrived, at
    /// once for 0.
    ModelRequest {
        request: u32,
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        size: u64,
    },
    /// Bytes of model request `request`'s body, in the frame's body, which go no further than
    /// its size. An empty frame changes nothing.
    ModelData { request: u32 },
}

/// How to start a command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub argv: Argv,
    /// Variables set over the environment `sandboxd` itself was given.
    pub env: Env,
    pub cwd: AbsolutePath,
    /// The regular file the command's standard input reads from; without one, its standard
    /// input is empty.
    pub stdin: Option<AbsolutePath>,
    /// The file the command's standard output goes to, created or truncated.
    pub stdout: AbsolutePath,
    /// The file the command's standard error goes to, created or truncated.
    pub stderr: AbsolutePath,
}

/// A part of a tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    /// A directory, which may be empty.
    Dir { path: RelativePath },
    /// A regular file of `size` bytes, which follow in `Data` frames.
    File { path: RelativePath, size: u64 },
}

/// How a command ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    /// It exited with this code.
    Code(i32),
    /// A signal with this number killed it.
    Signal(i32),
}
