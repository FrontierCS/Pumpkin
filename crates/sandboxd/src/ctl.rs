//! `sandboxctl`, the shim through which callers in a sandbox make ops: the same binary as
//! `sandboxd`, run under the name `sandboxctl`. The output is a reply's JSON form, which
//! [`schema::sandbox::op`] describes.
//!
//! It connects to `sandboxd` at the sandbox's [control socket](CONTROL_SOCKET), and carries
//! each op over its own connection, as [`crate::ops`] describes.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use std::{fs, io};

use schema::frame;
use schema::ir::Name;
use schema::sandbox::CONTROL_SOCKET;
use schema::sandbox::op::{JobState, OpCall, OpReply, StatusReply};
use stdx::{Context, Deadline};

use crate::print_line;

mod call;
mod session;
mod wait;

/// What a caller is told when the connection to `sandboxd` closes without an answer, as
/// [`crate::ops`] says it may.
const UNANSWERED: &str =
    "sandboxd closed the connection without an answer: expected one while its session lasts";

mod flags {
    #![expect(
        unreachable_pub,
        reason = "xflags declares the types it generates `pub`"
    )]
    #![expect(
        clippy::allow_attributes,
        clippy::allow_attributes_without_reason,
        reason = "xflags generates `#[allow(dead_code)]`, without a reason, on its parsers"
    )]

    use std::ffi::OsString;
    use std::num::NonZeroU32;

    xflags::xflags! {
        src "./src/ctl.rs"

        /// Makes an op through the sandbox's sandboxd, and prints the reply as one line of
        /// JSON, whose `kind` names it: `ops`, `call`, `status`, `lease`,
        /// `lease_status` or `refused`. Exits with 0, with 2 for the status of a job that gave
        /// no result, and with 1 for a refused op.
        /// Reports a local wait timeout on standard error and exits with 124. Reports any
        /// other error on standard error, with nothing printed, and exits with 1.
        cmd sandboxctl {
            /// Prints this sandbox's limits: the time left to work, its CPU and memory, and
            /// the grants it holds.
            cmd ops {
                /// Prints shell usage for this role's grants, including their arguments and limits.
                optional --text
            }

            /// Calls a function that this sandbox's role holds: `<function> [<file>]`, with the
            /// payload in <file> (empty when omitted), or `<session>.<op>`. A session’s ops answer
            /// at once and drive its runners one command or copy at a time: `open`
            /// opens a lease, whose runner lasts as the session lets it; `run <lease> [--timeout
            /// <seconds>] -- <program> [<arg>...]` runs a command in its runner, until the timeout
            /// or the lease's end; `put <lease> <tree> <to>` copies the directory <tree> here to
            /// <to> in the runner, and `get <lease> <from> <to>` the runner's <from> to <to> here;
            /// `status <lease>` shows the lease, how its last command or copy ended, and what is
            /// left; `close <lease>` ends it. A lease is named by the number `open` returned.
            cmd call {
                /// The function's name, as `ops` lists it, or a session's, a dot, and the op.
                required function: OsString
                /// The file that holds the payload, or a symbolic link to it; for a session's op,
                /// its arguments. A copied tree's root becomes <to>, which must not exist unless a
                /// put replaces it, a path in the runner given whole, from `/`; a copy keeps the
                /// permission bits of each file and directory, and gives a directory's owner all
                /// three; a copy counts as a command.
                repeated args: OsString
                /// With `run`, the most seconds the command may run, at most what the lease has
                /// left.
                optional --timeout seconds: OsString
                /// With `open`, the seconds the lease lasts once its runner is ready, at most what
                /// its session lets a lease last.
                optional --seconds seconds: OsString
                /// With `put`, removes whatever is at <to> in the runner first.
                optional --replace
                /// Waits up to this many seconds for a function's result. Retries an uncharged
                /// busy call, then follows the admitted job by its number. Prints the terminal
                /// status, or a now function's immediate reply. A timeout leaves an admitted job
                /// running. If the admission reply was lost, the call may have started.
                optional --wait seconds: NonZeroU32
                /// With a function's call, one of its caller's arguments, as `<name>=<value>`,
                /// read as `ops` lists its type.
                repeated --arg arg: OsString
            }

            /// Shows how a job that a call of a job function started is doing.
            cmd status {
                /// The job's number, as its call returned it.
                required job: OsString
            }
        }
    }
    // generated start
    // The following code is generated by `xflags` macro.
    // Run `env UPDATE_XFLAGS=1 cargo build` to regenerate.
    #[derive(Debug)]
    pub struct Sandboxctl {
        pub subcommand: SandboxctlCmd,
    }

    #[derive(Debug)]
    pub enum SandboxctlCmd {
        Ops(Ops),
        Call(Call),
        Status(Status),
    }

    #[derive(Debug)]
    pub struct Ops {
        pub text: bool,
    }

    #[derive(Debug)]
    pub struct Call {
        pub function: OsString,
        pub args: Vec<OsString>,

        pub timeout: Option<OsString>,
        pub seconds: Option<OsString>,
        pub replace: bool,
        pub wait: Option<NonZeroU32>,
        pub arg: Vec<OsString>,
    }

    #[derive(Debug)]
    pub struct Status {
        pub job: OsString,
    }

    impl Sandboxctl {
        #[allow(dead_code)]
        pub fn from_env_or_exit() -> Self {
            Self::from_env_or_exit_()
        }

        #[allow(dead_code)]
        pub fn from_env() -> xflags::Result<Self> {
            Self::from_env_()
        }

        #[allow(dead_code)]
        pub fn from_vec(args: Vec<std::ffi::OsString>) -> xflags::Result<Self> {
            Self::from_vec_(args)
        }
    }
    // generated end
}

/// Runs `sandboxctl` with `args`, its arguments after its name: its exit code is the op's
/// outcome, which its output doesn't change ([`print_line`]).
pub(crate) fn main(args: Vec<OsString>) -> ExitCode {
    let reply = match flags::Sandboxctl::from_vec(args) {
        Ok(flags) => {
            let text = matches!(
                flags.subcommand,
                flags::SandboxctlCmd::Ops(flags::Ops { text: true })
            );
            match run(flags) {
                Ok(OpReply::Ops(ops)) if text => {
                    print_line(io::stdout(), ops.guide().trim_end());
                    return ExitCode::SUCCESS;
                }
                result => result,
            }
        }
        Err(error) if error.is_help() => {
            print_line(io::stdout(), error);
            return ExitCode::SUCCESS;
        }
        // Bad arguments exit with 1, as sandboxctl's other errors do, and not with the 2 that
        // xflags exits with, which here means a job that gave no result.
        Err(error) => Err((1, format!("{error}: see `sandboxctl --help`"))),
    };
    match reply {
        Ok(reply) => {
            let Ok(json) = serde_json::to_string(&reply) else {
                unreachable!(
                    "{reply:?} can't be written as JSON, though serde_json fails only on a \
                     `Serialize` impl that fails or on a map key that isn't a string, and an \
                     `OpReply` has neither: its impls are derived, serde's or `Value`'s, and its \
                     maps have string keys"
                )
            };
            print_line(io::stdout(), json);
            ExitCode::from(exit_code(&reply))
        }
        Err((code, message)) => {
            print_line(io::stderr(), format_args!("sandboxctl: {message}"));
            ExitCode::from(code)
        }
    }
}

/// The exit code that `reply` gives, as `sandboxctl --help` lists them ([`flags`]).
fn exit_code(reply: &OpReply) -> u8 {
    match reply {
        OpReply::Ops(_) | OpReply::Call(_) | OpReply::Lease(_) | OpReply::LeaseStatus(_) => 0,
        OpReply::Status(StatusReply { state, .. }) => match state {
            JobState::Queued | JobState::Running | JobState::Completed { .. } => 0,
            JobState::Errored { .. } => 2,
        },
        OpReply::Refused { .. } => 1,
    }
}

fn run(flags: flags::Sandboxctl) -> Result<OpReply, (u8, String)> {
    let wait = match &flags.subcommand {
        flags::SandboxctlCmd::Call(call) => call
            .wait
            .map(|seconds| Deadline::after(Duration::from_secs(u64::from(seconds.get())))),
        flags::SandboxctlCmd::Ops(_) | flags::SandboxctlCmd::Status(_) => None,
    };
    let (call, body) = parse(flags).map_err(|message| (1, message))?;
    if let Some(by) = wait {
        return wait::call(&call, &body, by);
    }
    let unreached = format_args!("sandboxd at {CONTROL_SOCKET} can't be reached");
    let connection = UnixStream::connect(CONTROL_SOCKET)
        .context(unreached)
        .map_err(|error| {
            (
                1,
                format!("{error}: run sandboxctl in a sandbox that serves ops"),
            )
        })?;
    exchange(connection, &call, &body)
        .and_then(|answer| answer)
        .map_err(|message| (1, message))
}

fn exchange(
    mut connection: impl io::Read + io::Write,
    call: &OpCall,
    body: &[u8],
) -> Result<Result<OpReply, String>, String> {
    frame::write(&mut connection, call, body).context("the op could not be sent to sandboxd")?;
    match frame::read::<Result<OpReply, String>>(&mut connection) {
        Ok(Some((answer, _))) => Ok(answer),
        Ok(None) => Err(UNANSWERED.to_owned()),
        Err(error) => Err(error).context("sandboxd's answer can't be read"),
    }
}

/// Parses the arguments into the op's call and the request's body: for a function's call, the
/// payload file's absolute path ([`payload_path`]).
fn parse(flags: flags::Sandboxctl) -> Result<(OpCall, Vec<u8>), String> {
    match flags.subcommand {
        flags::SandboxctlCmd::Ops(_) => Ok((OpCall::Ops, Vec::new())),
        flags::SandboxctlCmd::Call(call) => call::call(call),
        flags::SandboxctlCmd::Status(flags::Status { job }) => {
            let job = job
                .to_str()
                .and_then(|job| job.parse().ok())
                .ok_or_else(|| {
                    format!(
                        "`{}` is not a job: name one by the number a call returned",
                        job.display()
                    )
                })?;
            Ok((OpCall::Status { job }, Vec::new()))
        }
    }
}

fn parse_grant(grant: &OsStr) -> Result<Name, String> {
    grant
        .to_str()
        .ok_or_else(|| format!("`{}` is not a grant's name: use UTF-8", grant.display()))
        .and_then(|grant| Name::try_from(grant.to_owned()))
}

/// The payload file's canonical path, as the request's body: absolute, and through no symbolic
/// link.
fn payload_path(file: &Path) -> Result<Vec<u8>, String> {
    if file.as_os_str().is_empty() {
        return Err("the payload file's path is empty: name a file".to_owned());
    }
    // Absolute, since `sandboxd`'s working directory isn't the caller's, and through no link,
    // since `sandboxd` opens none (`sys::open_regular`). `sandboxctl` runs as its caller, so
    // resolving links here reaches nothing the caller couldn't reach without it.
    let path = fs::canonicalize(file).map_err(|error| {
        format!(
            "{}: {error}: name a file this user can reach",
            file.display()
        )
    })?;
    Ok(path.as_os_str().as_bytes().to_vec())
}
