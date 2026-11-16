//! The sandbox protocol, v0: what the gateway and `sandboxd` say to each other over a session, and
//! the contract of the sandbox that a role's commands run in.
//!
//! A session is one connection, which lasts as long as its sandbox and identifies it: to a socket
//! of the sandbox's own, by a hello that presents its [`RegistrationGrant`], or the gateway's to
//! its endpoint ([`Listening`]). `sandboxd` sends a [`Hello`] first, and the gateway answers with a
//! [`HelloReply`], accepting its own [`PROTOCOL_VERSION`] alone. Every version keeps that order,
//! the [`frame`] encoding, a hello's `version` and size ([`MAX_HELLO_BYTES`]), a reply's `type` and
//! a refusal's `message`.
//!
//! Both sides send [`frame`]s: [`ToSandbox`] from the gateway, [`FromSandbox`] from `sandboxd`; a
//! reply names its request's stream, a number the gateway uses once. Files move as trees of
//! [`Entry`]s, each a directory or a regular file whose bytes follow in `Data` frames, after its
//! parent's, then an `End`, so no link or special file crosses a session, and every path is a
//! [`RelativePath`]; each entry keeps its [`Mode`] ([`tree_mode`]).
//!
//! Ops go the other way ([`op`]): [`FromSandbox::Op`] gives a payload's size before its bytes
//! follow in [`FromSandbox::OpData`], and the gateway answers once ([`ToSandbox::OpReply`]), at
//! most one op in flight a sandbox. A session takes ops only if accepted with [`OpPayloadLimit`],
//! and model requests ([`FromSandbox::ModelRequest`]) only if accepted for them.
//!
//! Every provider presents [`pumpkin_dir()`] as [`pumpkin_subdirs()`] says, within
//! [`PUMPKIN_BYTES`]. A role that makes ops has `sandboxctl` at [`SANDBOXCTL_PATH`], served at
//! [`CONTROL_SOCKET`]; a role with a model route has the model endpoint at [`MODEL_PORT`].

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use op::{OpCall, OpReply};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::ir::values::RelativePath;
use crate::ir::{AbsolutePath, Argv, Env, EnvVar, GpuModel, Mode};

pub mod frame;
pub mod op;
pub mod tree;

/// The most bytes an op's payload may hold, whatever a session's [`OpPayloadLimit`]: a sandbox
/// that opens an op with more breaks the protocol.
pub const MAX_OP_PAYLOAD_BYTES: u64 = 64 << 20;

/// The most bytes of files a transfer of a tree between two sandboxes may move, which bounds
/// what the host holds of it on its disk
/// ([`transfer_bytes`](crate::ir::Session::transfer_bytes)).
// As much as a sandbox's `/pumpkin` holds, which bounds every tree the host collects into a run's
// record.
pub const MAX_TRANSFER_BYTES: u64 = PUMPKIN_BYTES;

/// The most bytes an op's payload may hold in a session that takes ops, which the gateway accepts
/// it with: at most [`MAX_OP_PAYLOAD_BYTES`]. A payload over it is refused once it has arrived
/// whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpPayloadLimit(pub(crate) u64);

impl OpPayloadLimit {
    /// No bytes: a session whose ops carry no payload.
    pub const NONE: Self = Self(0);

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

/// The most model requests a sandbox may have in flight, whatever client makes them, which bounds
/// what the sandbox's requests cost the host.
// A lead agent and six helpers, each waiting on one response at a time.
pub const MAX_MODEL_REQUESTS_IN_FLIGHT: usize = 7;

/// The most bytes a model request's body may hold: as many as an op's payload, since the gateway
/// holds each whole, as it holds a payload.
pub const MAX_MODEL_REQUEST_BYTES: u64 = MAX_OP_PAYLOAD_BYTES;

/// The directory that holds Pumpkin's files in every sandbox, `/pumpkin`, and belongs to the
/// role's user.
///
/// The session's owner puts a role's inputs there and collects its results from there; a role's
/// commands read and write them at the paths below it.
pub fn pumpkin_dir() -> AbsolutePath {
    const { AbsolutePath::from_static("/pumpkin") }
}

/// The directories below [`pumpkin_dir()`], there before a role's first command starts: the
/// in-sandbox contract that a task's images are written against, the same for every workflow.
///
/// `in` holds what the session's owner puts in, as the agent's standard input
/// ([`command_stdin()`]); [`out_dir()`] what it collects, as the agent's output; `home` the
/// commands' home ([`command_env()`]); and [`log_dir()`] their output ([`command_stdout()`],
/// [`command_stderr()`]). These paths are relative to [`pumpkin_dir()`].
pub fn pumpkin_subdirs() -> [RelativePath; 4] {
    [in_dir(), out_dir(), home_dir(), log_dir()]
}

/// The mode of each directory the session's owner puts below [`pumpkin_dir()`].
pub fn dir_mode() -> Mode {
    Mode::new(0o755)
}

/// The mode of each file the session's owner puts below [`pumpkin_dir()`].
pub fn file_mode() -> Mode {
    Mode::new(0o644)
}

fn in_dir() -> RelativePath {
    const { RelativePath::from_static("in") }
}

fn home_dir() -> RelativePath {
    const { RelativePath::from_static("home") }
}

pub fn out_dir() -> RelativePath {
    const { RelativePath::from_static("out") }
}

pub fn log_dir() -> RelativePath {
    const { RelativePath::from_static("log") }
}

/// The most bytes a body may get from its target in one file: a larger one is refused whole
/// rather than cut short.
// The host collects the file, then reads it whole into memory for the body, which may parse it
// as a document: the bound keeps what the host holds for it small, and a document cut short is no
// document. 1 MiB holds a result that a body returns fields of with room to spare.
pub const RESULT_MAX_BYTES: u64 = 1 << 20;

/// How deep a body's return may nest lists, tuples and dicts, the returned dict included: a
/// deeper one is no result.
// A run's record nests what a body returns inside objects of its own, a log line's `result` and
// `returned` among them, and serde_json reads at most 127 nested arrays and objects: this bound
// leaves 7 of them for what holds a return, so that every line of a run's log reads back.
pub const RESULT_MAX_DEPTH: usize = 127 - 7;

/// The environment that a role's command, whose IR gives it `env`, runs with: `HOME` first, set to
/// `home` below [`pumpkin_dir()`], then `env`; or `env` alone, if it sets `HOME` itself.
pub fn command_env(env: &Env) -> Env {
    env.with_default(EnvVar::home(&pumpkin_dir().join(&home_dir())))
}

pub fn command_stdout() -> RelativePath {
    log_dir().join(&const { RelativePath::from_static("command.stdout") })
}

pub fn command_stderr() -> RelativePath {
    log_dir().join(&const { RelativePath::from_static("command.stderr") })
}

/// `in/command.stdin`, which the agent's command reads its standard input from, if its runtime
/// gives one ([`AgentRuntime::stdin`](crate::ir::AgentRuntime::stdin)).
pub fn command_stdin() -> RelativePath {
    in_dir().join(&const { RelativePath::from_static("command.stdin") })
}

/// The most [`pumpkin_dir()`] holds, in bytes, so that whatever the session's owner collects from
/// below it fits in this many.
///
/// A provider may back it with the role's memory, so that its files and the role's processes share
/// the role's memory limit: a role can keep there at most the smaller of the two.
pub const PUMPKIN_BYTES: u64 = 1 << 30;

/// Where every provider presents `sandboxd`'s binary in a sandbox, `/.pumpkin/sandboxd`, which it
/// starts there.
pub const SANDBOXD_PATH: &str = "/.pumpkin/sandboxd";

/// Where `sandboxctl` is in a sandbox whose role makes ops, `/.pumpkin/bin/sandboxctl`.
///
/// Every provider presents `sandboxd`'s binary there, for the role's user to execute, under the
/// file name that makes it run as `sandboxctl`. [`op`] gives what its commands print.
pub const SANDBOXCTL_PATH: &str = "/.pumpkin/bin/sandboxctl";

/// The control socket, where `sandboxd` serves ops in a sandbox whose role makes ops,
/// `/pumpkin/sandboxctl/socket`.
///
/// Every provider starts that sandbox's `sandboxd` with `--ops`, which makes it serve them there,
/// below [`pumpkin_dir()`], and `sandboxctl` connects there.
pub const CONTROL_SOCKET: &str = "/pumpkin/sandboxctl/socket";

/// The directories of a sandbox that hold Pumpkin's own files: [`pumpkin_dir()`], and `/.pumpkin`,
/// where providers present `sandboxd`, and `sandboxctl` at [`SANDBOXCTL_PATH`].
pub(crate) fn pumpkin_dirs() -> [AbsolutePath; 2] {
    [
        pumpkin_dir(),
        const { AbsolutePath::from_static("/.pumpkin") },
    ]
}

/// The port of the model endpoint, on `127.0.0.1` in a sandbox whose role has a model route, at
/// [`model_url()`].
///
/// Every provider starts that sandbox's `sandboxd` with `--model`, which makes it serve the
/// endpoint there, and the session's owner accepts the session to take model requests.
pub const MODEL_PORT: u16 = 11235;

/// The URL of the model endpoint, `http://127.0.0.1:<MODEL_PORT>`.
///
/// A harness in the sandbox makes model requests ([`FromSandbox::ModelRequest`]) there: by that
/// address, not `localhost`, which may also name the IPv6 loopback, where nothing listens.
pub fn model_url() -> String {
    format!("http://127.0.0.1:{MODEL_PORT}")
}

/// The version of the protocol that both ends of a session speak, which its [`Hello`] gives.
pub const PROTOCOL_VERSION: u32 = 0;

/// The most bytes a [`Hello`]'s message holds, at any version ([the protocol](crate::sandbox)); its
/// body holds none.
// Room for the names of 8 GPUs, the most that any provider gives one sandbox, each of fewer than
// the 96 bytes that `nvidia-smi` names a device in, quoted and followed by a comma, beside the rest
// of the hello, which takes under 128.
pub const MAX_HELLO_BYTES: usize = 1024;
const _: () = assert!(8 * (96 + 3) + 128 <= MAX_HELLO_BYTES);

/// How long either end gives the other to send its hello, or to answer it, from when it takes the
/// connection: the gateway that a sandbox dials, or a `sandboxd` that listens.
// A `sandboxd` sends its hello as it connects, and the gateway answers at once, so this waits
// for the network alone: a hello lost three times still arrives within it, resent at TCP's first
// retransmission timeout, 1 s, and then at twice the wait before each time.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// What `sandboxd` sends first on every session; [`frame::read_hello`] reads one.
///
/// Its body is empty, and its message `{"version":v}`, with `v` its [`PROTOCOL_VERSION`], and
/// `"grant":"…"` where it presents a grant, and `"gpus":[…]` where it saw GPUs.
///
/// Its `Deserialize` refuses a hello of another version for its version, whatever else it holds,
/// then a field that this version's form lacks, a grant that isn't one or a GPU's model that isn't
/// one, and a frame that gives no version.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(into = "HelloJson", try_from = "Value")]
pub struct Hello {
    /// The grant the sandbox presents where it dials an address that several sandboxes share; none
    /// on a socket of its own.
    pub grant: Option<RegistrationGrant>,
    /// The GPUs that `sandboxd` saw as it started, each by its model as `nvidia-smi` names it; none
    /// where it couldn't tell, as in a sandbox without a GPU, or where their names would make the
    /// hello longer than [`MAX_HELLO_BYTES`].
    pub gpus: Option<Vec<GpuModel>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloJson {
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant: Option<RegistrationGrant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpus: Option<Vec<GpuModel>>,
}

impl From<Hello> for HelloJson {
    fn from(Hello { grant, gpus }: Hello) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            grant,
            gpus,
        }
    }
}

impl TryFrom<Value> for Hello {
    type Error = String;

    fn try_from(json: Value) -> Result<Self, Self::Error> {
        #[derive(Deserialize)]
        #[serde(expecting = "an object with a `version`")]
        struct Versioned {
            version: u32,
        }
        // From a `Value`, whose errors give no position, unlike a reader's, which would put one at
        // the end of each refusal.
        let Versioned { version } = Versioned::deserialize(&json).map_err(|error| {
            format!("the session's first frame is no hello: {error}: open the session with one")
        })?;
        if version != PROTOCOL_VERSION {
            return Err(format!(
                "sandboxd speaks sandbox protocol {version}: expected {PROTOCOL_VERSION}, which \
                 this gateway speaks, as a sandboxd built from the same Pumpkin does"
            ));
        }
        let HelloJson {
            version: _,
            grant,
            gpus,
        } = HelloJson::deserialize(json).map_err(|error| {
            format!("the hello isn't of sandbox protocol {PROTOCOL_VERSION}'s form: {error}")
        })?;
        Ok(Self { grant, gpus })
    }
}

/// A registration grant: the secret by which a sandbox that dials an address several sandboxes
/// share tells the gateway which it is.
///
/// The gateway issues each, of random bytes, for one sandbox's start, and accepts it once, in that
/// sandbox's [`Hello`], while the start waits: it opens one session. It is
/// [`REGISTRATION_GRANT_BYTES`] bytes, written as twice as many lowercase hexadecimal digits. Its
/// `==` takes the same time whichever of its bytes differ. Its `Debug` shows none of them, it has
/// no `Display`, and the error that refuses digits quotes none.
#[derive(Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct RegistrationGrant([u8; REGISTRATION_GRANT_BYTES]);

pub const REGISTRATION_GRANT_BYTES: usize = 16;

impl From<[u8; REGISTRATION_GRANT_BYTES]> for RegistrationGrant {
    fn from(bytes: [u8; REGISTRATION_GRANT_BYTES]) -> Self {
        Self(bytes)
    }
}

impl TryFrom<String> for RegistrationGrant {
    type Error = String;

    fn try_from(digits: String) -> Result<Self, Self::Error> {
        // The error quotes none of the digits, which may be a grant's.
        let refused = || {
            format!(
                "the registration grant isn't {} lowercase hexadecimal digits: present the one \
                 the gateway issued for the sandbox",
                2 * REGISTRATION_GRANT_BYTES
            )
        };
        let value = |digit: u8| match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            _ => None,
        };
        if digits.len() != 2 * REGISTRATION_GRANT_BYTES {
            return Err(refused());
        }
        let mut bytes = [0; REGISTRATION_GRANT_BYTES];
        for (byte, &[high, low]) in bytes.iter_mut().zip(digits.as_bytes().as_chunks().0) {
            let (Some(high), Some(low)) = (value(high), value(low)) else {
                return Err(refused());
            };
            *byte = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl RegistrationGrant {
    /// Its digits, as [`SANDBOXD_GRANT`] holds them, and its JSON form writes them.
    pub fn digits(&self) -> String {
        crate::ir::values::hex(&self.0)
    }
}

impl Serialize for RegistrationGrant {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.digits())
    }
}

impl PartialEq for RegistrationGrant {
    fn eq(&self, other: &Self) -> bool {
        // Every byte is compared, without a branch on any, so that a peer that presents grants
        // learns nothing of an issued one from how long the gateway takes to refuse its own.
        // `black_box` keeps the optimizer from compiling the fold to an equality that stops at the
        // first byte that differs.
        let differ = (self.0.iter().zip(&other.0)).fold(0, |differ, (a, b)| differ | (a ^ b));
        core::hint::black_box(differ) == 0
    }
}

impl Eq for RegistrationGrant {}

impl fmt::Debug for RegistrationGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // None of its digits, so that no message or log carries a grant by accident.
        f.write_str("RegistrationGrant(..)")
    }
}

/// Where a sandbox's `sandboxd` dials the gateway on the gateway's host: the Unix socket at
/// [`SOCKET_NAME`] in `dir`, which the gateway issued for this sandbox (`gateway::Sockets`).
///
/// The gateway listens there from before the create, since `sandboxd` dials once, as it starts. The
/// socket takes the first connection, whoever makes it, so the provider shows `dir` to this
/// sandbox alone; the role's user needs to traverse only `dir`.
#[derive(Debug)]
pub struct SocketDial {
    pub dir: PathBuf,
}

/// Where a sandbox's `sandboxd` dials the gateway on a private network, `gateway`, and the grant
/// its hello presents there, issued for this sandbox alone (`gateway::GrantListener`).
///
/// The gateway listens before the create, and `sandboxd` dials once as it starts: `gateway` its
/// argument, `grant`'s digits in [`SANDBOXD_GRANT`]. The grant opens the session for the first
/// hello that presents it, so the provider shows it to this `sandboxd` alone: in its environment,
/// never in an argument, a log or the console, nothing else running as the role's user until the
/// session opens. The session is cleartext, so `gateway` is on a private network of the run's.
#[derive(Debug)]
pub struct TcpDial {
    pub gateway: SocketAddr,
    pub grant: RegistrationGrant,
}

/// What a sandbox whose `sandboxd` listens is given: the grant that the gateway's answer to its
/// hello presents.
///
/// `sandboxd` listens, with `--listen` and an address of the sandbox's own, and `--by` the start's
/// deadline, `grant`'s digits kept as for a [`TcpDial`], and the gateway dials it after the create,
/// until that deadline, at the [`Endpoint`] the create gives, behind TLS that the provider ends
/// with a Web PKI certificate for the endpoint's name. `sandboxd` opens the session with the first
/// peer whose answer to its hello presents `grant` ([`HelloReply`]), and TLS keeps the grant from
/// any but the provider.
#[derive(Debug)]
pub struct Listening {
    pub grant: RegistrationGrant,
}

/// Where the gateway dials a sandbox whose `sandboxd` listens ([`Listening`]): a DNS name, which
/// its provider's TLS certificate names, and a TCP port, such as `ta-1.w.modal.host:443`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    /// The endpoint at `host`, a DNS name, as letters, digits and `-` in labels of 1 to 63
    /// characters, neither starting nor ending with `-`, joined by `.`, at most 253 in all, the
    /// last not all digits, and `port`, which isn't 0.
    ///
    /// Fails if `host` isn't such a name, or `port` is 0.
    pub fn new(host: String, port: u16) -> Result<Self, String> {
        let label = |label: &str| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        };
        // A last label of digits alone would make an IPv4 address of the name, which no certificate
        // for a name is.
        let numeric = host
            .rsplit('.')
            .next()
            .is_some_and(|last| last.bytes().all(|byte| byte.is_ascii_digit()));
        if host.len() > 253 || !host.split('.').all(label) || numeric {
            return Err(format!(
                "`{}` is not a DNS name: give the name the endpoint's certificate is for, such \
                 as `ta-1.w.modal.host`",
                host.escape_debug()
            ));
        }
        if port == 0 {
            return Err(format!(
                "port 0 of `{host}` is no endpoint's: give the port it serves"
            ));
        }
        Ok(Self { host, port })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// The name of a [`SocketDial`]'s socket in its directory.
pub const SOCKET_NAME: &str = "s";

/// The environment variable that holds, as its digits, the [`RegistrationGrant`] that a `sandboxd`
/// presents in its hello where it dials its gateway at a TCP address.
///
/// A provider that starts `sandboxd` for a [`TcpDial`] or [`Listening`] sets it, and `sandboxd`
/// keeps it from every command it starts.
// The environment, not `argv`, which every process may read through `/proc`: a process's
// environment is open only to those that may trace it. The role's code runs as `sandboxd`'s user,
// which may until `sandboxd` is non-dumpable, and may keep a handle opened by then, so what keeps
// the grant from that code is that none of it runs before the hello, which spends the grant:
// `sandboxd` starts each command after it, and before it only the image's `nvidia-smi`, to name the
// sandbox's GPUs, without this variable, once it is non-dumpable; and the provider starts nothing
// else as the role's user before it (`TcpDial`, `Listening`).
pub const SANDBOXD_GRANT: &str = "SANDBOXD_GRANT";

/// The gateway's answer to the session's first frame, with an empty body: `{"type":"accepted"}`, or
/// `{"type":"accepted","grant":"…"}` where the gateway dialed, or a refusal.
///
/// Its `Deserialize` reads a reply's `type`, and a refusal's `message`, whatever else it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// Without `deny_unknown_fields`, so that `sandboxd` reads a refusal from a gateway of any version,
// whose form may hold more.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelloReply {
    /// The session is open: the gateway's requests follow. Where the gateway dialed a `sandboxd`
    /// that listens, it presents the sandbox's `grant`, and `sandboxd` serves no peer that presents
    /// another, or none; elsewhere it presents none.
    Accepted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<RegistrationGrant>,
    },
    /// The gateway refuses the session, for the reason `message` gives, and closes the connection.
    Refused { message: String },
}

/// What the gateway sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToSandbox {
    /// Writes a tree into `dir`, made as `root` says. The tree's entries follow on the same stream,
    /// and `sandboxd` replies once: when the tree has been written, or at the first failure.
    Put {
        stream: u32,
        dir: AbsolutePath,
        root: Root,
    },
    /// An entry of the tree being put ([`tree::Writer::entry`]).
    Entry { stream: u32, entry: Entry },
    /// Bytes of the file being put, in the frame's body.
    Data { stream: u32 },
    /// The end of the tree being put.
    End { stream: u32 },
    /// Starts a command in a process group of its own. `sandboxd` replies when it has started, and
    /// later sends [`FromSandbox::Exited`] on the same stream.
    Start { stream: u32, launch: Launch },
    /// Kills the process group of the command started on stream `command`, if the command still
    /// runs, and replies once it has signalled it; the command's exit follows on its own stream. A
    /// process that left the group runs on until a freeze.
    Kill { stream: u32, command: u32 },
    /// Kills every process that the role's user runs in the sandbox but `sandboxd`, which is every
    /// process but `sandboxd` where `sandboxd` is the sandbox's first process, and refuses to start
    /// more, so that what is collected afterwards can't change. A platform's own first process,
    /// such as Modal's `dumb-init`, runs on. `sandboxd` replies once they are gone.
    Freeze { stream: u32 },
    /// Sends the tree at `path`, a directory's entries, then an `End`. A regular file at `path`
    /// is sent as the tree of that one file, by its name.
    Collect { stream: u32, path: AbsolutePath },
    /// The answer to op `op`, which ends it.
    OpReply { op: u32, reply: OpReply },
    /// The status and headers of the response to model request `request`, sent once the request is
    /// whole. Its body follows in `ModelData` frames, then a `ModelEnd`.
    ModelResponse {
        request: u32,
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// Bytes of the body of the response to model request `request`, in the frame's body.
    ModelData { request: u32 },
    /// The end of the response to model request `request`, which takes the request out of flight:
    /// whole, or `cut` short, which its client must not take as whole. With no `ModelResponse`
    /// before it, the request ends without a response, cut short.
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
    /// [`MAX_OP_PAYLOAD_BYTES`], follows in `OpData` frames. Only a call that takes one
    /// ([`OpCall::takes_payload`]) has one; `size` is 0 for any other. The op is whole once `size`
    /// bytes of `OpData` have arrived, at once for 0, and the gateway answers it after that.
    Op { op: u32, call: OpCall, size: u64 },
    /// Bytes of op `op`'s payload, in the frame's body, which go no further than its size. An empty
    /// frame changes nothing.
    OpData { op: u32 },
    /// Opens model request `request` for a client of the model endpoint that `sandboxd` serves for
    /// a role with a model route: an HTTP request with the method, path and headers the client
    /// sent, the headers' names in lowercase. Its number is none that a model request in flight
    /// has, in a number space apart from ops', and it is in flight from here to its response's
    /// [`ToSandbox::ModelEnd`]: at most [`MAX_MODEL_REQUESTS_IN_FLIGHT`] are. Its body, of `size`
    /// bytes, at most [`MAX_MODEL_REQUEST_BYTES`], follows in `ModelData` frames, as an op's
    /// payload does, and the gateway responds once all of it has arrived, at once for 0.
    ModelRequest {
        request: u32,
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        size: u64,
    },
    /// Bytes of model request `request`'s body, in the frame's body, which go no further than its
    /// size. An empty frame changes nothing.
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
    /// The regular file the command's standard input reads from; without one, its standard input is
    /// empty.
    pub stdin: Option<AbsolutePath>,
    /// The file the command's standard output goes to, created or truncated.
    pub stdout: AbsolutePath,
    /// The file the command's standard error goes to, created or truncated.
    pub stderr: AbsolutePath,
}

/// How a put makes the directory it writes its tree into, with any of its parents missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    /// Made unless it exists, which is kept as it is.
    Kept,
    /// Made new: the put fails if it exists.
    New,
    /// Made new, whatever was there removed first.
    Replaced,
}

/// A part of a tree, with its `mode`, as [`tree_mode`] keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    /// A directory, which may be empty.
    Dir { path: RelativePath, mode: Mode },
    /// A regular file of `size` bytes, which follow in `Data` frames.
    File {
        path: RelativePath,
        size: u64,
        mode: Mode,
    },
}

/// The mode a tree keeps of an entry whose `st_mode` is `st_mode`: its permission bits.
///
/// Read, write and execute, for its owner, its group and others, mean the same where the receiving
/// side's user owns the entry, as it owns all it writes. Ownership, and the set-user-ID,
/// set-group-ID and sticky bits, which a sender could plant to its receiver's harm, never cross.
pub fn tree_mode(st_mode: u32) -> Mode {
    Mode::new((st_mode & 0o777) as u16)
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
