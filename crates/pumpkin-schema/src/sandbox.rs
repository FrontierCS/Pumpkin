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
//! which can't leave the directory the tree is written to; [`tree::Writer`] writes one there.
//!
//! Ops go the other way: a caller in the sandbox asks the session's owner for something, such
//! as judging a payload ([`op`]). An op opens with [`FromSandbox::Op`], numbered apart from the
//! gateway's streams, and gives its payload's size first, so the gateway checks the size before
//! any byte arrives; the bytes follow in [`FromSandbox::OpData`], and the gateway answers once,
//! with [`ToSandbox::OpReply`]. A sandbox has at most one op in flight, from its `Op` to its
//! reply, so its callers take turns, and the owner answers each promptly: an op that starts
//! long work, as a judge call does, with a job that later ops ask about. A session takes ops
//! only once its owner enables them, before the sandbox's first command.
//!
//! Every provider presents [`pumpkin_dir()`], with [`pumpkin_subdirs()`] below it, in the
//! sandboxes it starts: the driver and a task's commands rely on them in any sandbox. It holds
//! at most [`PUMPKIN_BYTES`], or less where a provider backs it with the role's memory. A
//! sandbox whose role makes ops also has `sandboxctl` at [`SANDBOXCTL_PATH`], and its
//! `sandboxd` serves ops at [`CONTROL_SOCKET`]. Any other sandbox is passive: nothing in it
//! can make an op, and its session takes none.

use serde::{Deserialize, Serialize};

pub use crate::ir::values::RelativePath;
use crate::ir::{AbsolutePath, Argv, Env};
use op::{OpCall, OpReply};

pub mod frame;
pub mod op;
pub mod tree;

/// The most bytes an op's payload may hold. `sandboxd` refuses a longer payload before it
/// opens the op, and no session's [`OpPayloadLimit`] is higher.
pub const MAX_OP_PAYLOAD_BYTES: u64 = 64 << 20;

/// The most bytes an op's payload may hold in a session, which its owner enables ops with: at
/// most [`MAX_OP_PAYLOAD_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpPayloadLimit(u64);

impl OpPayloadLimit {
    /// The limit, in bytes.
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

/// The directory that holds Pumpkin's files in every sandbox, `/pumpkin`, and belongs to the
/// role's user.
///
/// The driver puts a role's inputs there and collects its results from there; a task's
/// commands read and write them at the paths below it.
pub fn pumpkin_dir() -> AbsolutePath {
    valid("/pumpkin")
}

/// The directories below [`pumpkin_dir()`], there before a role's first command starts: `in`
/// holds what the driver puts in, `out` what it collects, `home` the commands' home, and `log`
/// their output.
pub fn pumpkin_subdirs() -> [RelativePath; 4] {
    ["in", "out", "home", "log"].map(valid)
}

/// The most [`pumpkin_dir()`] holds, in bytes, so that whatever the driver collects from below
/// it fits in this many.
///
/// A provider may back it with the role's memory, as the Docker provider does, so that its
/// files and the role's processes share the role's memory limit: a role can keep there at most
/// the smaller of the two.
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

/// `path`, one of this module's constant paths, as the path type it is.
fn valid<T: TryFrom<String, Error = String>>(path: &str) -> T {
    match T::try_from(path.to_owned()) {
        Ok(path) => path,
        #[expect(
            clippy::unreachable,
            reason = "each of this module's paths is normalized: `/pumpkin` is absolute, and \
                      each of its directories one plain component"
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
    /// An entry of the tree being put.
    Entry { stream: u32, entry: Entry },
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
}

/// How to start a command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub argv: Argv,
    /// Variables set over the environment `sandboxd` itself was given.
    pub env: Env,
    pub cwd: AbsolutePath,
    /// The file the command's standard output goes to, created or truncated. Its standard
    /// input is empty.
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
