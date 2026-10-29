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

use serde::{Deserialize, Serialize};

pub use crate::ir::values::RelativePath;
use crate::ir::{AbsolutePath, Argv, Env};

pub mod frame;
pub mod tree;

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
