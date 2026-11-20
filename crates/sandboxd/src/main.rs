//! `sandboxd`, the supervisor that runs in every sandbox, and, run under the file name
//! `sandboxctl`, the shim through which callers in the sandbox make ops ([`ctl`]).
//!
//! It meets the gateway, at a Unix socket of the sandbox's own, at a TCP address, or with
//! `--listen` where the gateway dials it, opens the session ([`schema::sandbox`]) with its hello,
//! presenting the grant in [`SANDBOXD_GRANT`] over TCP, and serves it until the gateway closes it.
//! When the session ends it kills its commands and exits, which stops a sandbox it is first in.
//!
//! As a sandbox's first process (PID 1) it reaps the orphans the kernel hands it, and freezing
//! kills every other process in the sandbox. Started with `--user`, behind a platform's own first
//! process, it asks for its descendants' orphans ([`sys::adopt_orphans`]), and freezing kills
//! every other process of the role's user. Run as any other process, as in tests, it kills only
//! the process groups of the commands still running.
//!
//! It runs as the role's user, needing no privileges, from the start or, started as root with
//! `--user`, from before it does anything else but give `/pumpkin` to that user
//! ([`sys::become_user`]). Everything it does, the role's own code could do too, so it is no
//! boundary within its sandbox. It does make itself non-dumpable first, so that the role's code
//! can't reach it through `/proc` ([`sys::make_non_dumpable`] says why).

use std::env;
use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, fchown};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use schema::ir::User;
use schema::sandbox::{HELLO_TIMEOUT, RegistrationGrant, SANDBOXD_GRANT, pumpkin_dir};
use stdx::{Context, Deadline};

use crate::session::Gateway;

/// How long past its start's deadline a `sandboxd` that listens waits for the gateway (`--by`):
/// four times the most a hello takes, which leaves a clock that runs ahead of the host's by up to
/// three times that no start that the gateway would complete.
const UNCLAIMED_SLACK: Duration = HELLO_TIMEOUT.saturating_mul(4);

mod commands;
mod ctl;
mod files;
mod model;
mod ops;
mod session;
mod sys;
mod until;

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

    use super::{Address, RoleUser};

    xflags::xflags! {
        src "./src/main.rs"

        /// Serves a sandbox's session as its supervisor, until the gateway ends it. Exits with
        /// 0 then, and with 1 on any error, bad arguments included.
        cmd sandboxd {
            /// The gateway's address, over which the session runs: the absolute path of the
            /// sandbox's own Unix socket, or a TCP address, such as `192.0.2.1:7433`. Over TCP,
            /// sandboxd presents the registration grant that the environment's SANDBOXD_GRANT
            /// holds, which it keeps from the commands it starts.
            required gateway: Address
            /// Listens at the gateway's address, a TCP address of the sandbox's own, such as
            /// `0.0.0.0:7434`, rather than dial it, until --by, and serves the first peer whose
            /// answer to its hello presents the grant that SANDBOXD_GRANT holds, giving each peer
            /// 15 s in all, and no time past that deadline, to take the hello and answer it.
            /// Refused with a Unix socket's path.
            optional --listen
            /// With --listen, which needs it, when its start is due by, in whole seconds since the
            /// Unix epoch, as the host's clock reads it: a sandboxd that no peer has claimed a
            /// minute after it exits with 1, since the gateway that would claim it gives up then,
            /// which ends a sandbox that its provider made too late. Refused without --listen.
            optional --by seconds: u64
            /// Becomes this user, `uid:gid`, each a number but 0, as sandboxd starts, which must be
            /// as root, having given the user /pumpkin, made if absent: no command, nor sandboxd,
            /// then keeps or regains a privilege. Freezing then kills every other process of the
            /// user, whether or not sandboxd is its sandbox's first process.
            optional --user user: RoleUser
            /// Serves ops, which callers in the sandbox make through `sandboxctl`, at
            /// /pumpkin/sandboxctl/socket. sandboxd makes its directory, which must not exist
            /// yet, closed to all but its own user. Without it, nothing in the sandbox can make
            /// an op.
            optional --ops
            /// Serves the model endpoint, through which a harness in the sandbox makes model
            /// requests, at http://127.0.0.1:11235. The gateway must accept the session to take
            /// them, or the first ends it. Without it, nothing in the sandbox can make one.
            optional --model
        }
    }
    // generated start
    // The following code is generated by `xflags` macro.
    // Run `env UPDATE_XFLAGS=1 cargo build` to regenerate.
    #[derive(Debug)]
    pub struct Sandboxd {
        pub gateway: Address,

        pub listen: bool,
        pub by: Option<u64>,
        pub user: Option<RoleUser>,
        pub ops: bool,
        pub model: bool,
    }

    impl Sandboxd {
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

#[expect(
    clippy::print_stderr,
    reason = "sandboxd reports why it stopped on its standard error, which the provider keeps"
)]
fn main() -> ExitCode {
    stdx::abort_on_panic();
    let mut args = std::env::args_os();
    let name = args.next().unwrap_or_default();
    let args: Vec<OsString> = args.collect();
    if Path::new(&name).file_name() == Some("sandboxctl".as_ref()) {
        return ctl::main(args);
    }
    let flags::Sandboxd {
        gateway,
        listen,
        by,
        user,
        ops,
        model,
    } = match flags::Sandboxd::from_vec(args) {
        Ok(flags) => flags,
        Err(error) if error.is_help() => {
            print_line(io::stdout(), error);
            return ExitCode::SUCCESS;
        }
        // Bad arguments exit with 1, as sandboxd's other errors do, and not with the 2 that
        // xflags exits with, which `--help` doesn't list.
        Err(error) => {
            eprintln!("sandboxd: {error}: see `sandboxd --help`");
            return ExitCode::FAILURE;
        }
    };
    if let Some(RoleUser(user)) = user
        && let Err(error) = become_role(user)
    {
        eprintln!("sandboxd: {error}");
        return ExitCode::FAILURE;
    }
    // After the user is become, as becoming one may make the process dumpable again.
    if let Err(error) = sys::make_non_dumpable() {
        eprintln!("sandboxd: making the process non-dumpable failed: {error}");
        return ExitCode::FAILURE;
    }
    // Past its start's deadline, the slack covers a hello begun just before it, which the
    // gateway gives `HELLO_TIMEOUT`, and clocks of the host and the sandbox that are apart.
    let by = by.map(|seconds| Deadline::at_unix(Duration::from_secs(seconds) + UNCLAIMED_SLACK));
    let gateway = match (gateway, listen, by) {
        (Address::Socket(socket), false, None) => Ok(Gateway::Socket(socket)),
        (Address::Socket(_), true, _) => Err(
            "--listen takes a TCP address of the sandbox's own: give one, such as `0.0.0.0:7434`, \
             or no --listen with a socket"
                .to_owned(),
        ),
        (Address::Tcp(address), false, None) => grant().map(|grant| Gateway::Tcp(address, grant)),
        (Address::Tcp(address), true, Some(by)) => {
            grant().map(|grant| Gateway::Listen(address, grant, by))
        }
        (Address::Tcp(_), true, None) => Err(
            "--listen waits until the start's deadline: give it as --by, in seconds since the Unix \
             epoch"
                .to_owned(),
        ),
        (Address::Socket(_) | Address::Tcp(_), false, Some(_)) => Err(
            "--by bounds how long a sandboxd that listens waits: give it with --listen".to_owned(),
        ),
    };
    match gateway.and_then(|gateway| session::serve(&gateway, ops, model)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sandboxd: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Where the command line says the gateway is: at the absolute path of a Unix socket of the
/// sandbox's own, or at a TCP address.
#[derive(Debug)]
enum Address {
    Socket(PathBuf),
    Tcp(SocketAddr),
}

impl FromStr for Address {
    type Err = String;

    fn from_str(address: &str) -> Result<Self, Self::Err> {
        // An absolute path begins with `/`, and a TCP address never does.
        if address.starts_with('/') {
            return Ok(Self::Socket(PathBuf::from(address)));
        }
        address.parse().map(Self::Tcp).map_err(|_| {
            format!(
                "`{address}` is neither a Unix socket's absolute path nor a TCP address: give \
                 one, such as `/run/gateway.sock` or `192.0.2.1:7433`"
            )
        })
    }
}

/// The user `--user` names, `uid:gid`.
#[derive(Debug)]
struct RoleUser(User);

impl FromStr for RoleUser {
    type Err = String;

    fn from_str(user: &str) -> Result<Self, Self::Err> {
        let id = |id: &str| id.parse::<NonZeroU32>().ok();
        match user.split_once(':').map(|(uid, gid)| (id(uid), id(gid))) {
            Some((Some(uid), Some(gid))) => Ok(Self(User { uid, gid })),
            _ => Err(format!(
                "`{user}` is not a user's `uid:gid`: give two numbers but 0, such as `1000:1000`"
            )),
        }
    }
}

/// Gives [`pumpkin_dir`] to `user`, made with mode 0755 if absent, then becomes `user`, and has
/// the kernel hand `sandboxd` the orphans of what it starts, as what starts it as root may put a
/// process of its own first in the sandbox. Refuses, before any of it, a process that isn't root.
fn become_role(User { uid, gid }: User) -> Result<(), String> {
    if !sys::is_root() {
        return Err(format!(
            "started as a user that isn't root: `--user` needs sandboxd to start as root to drop \
             to {uid}:{gid}"
        ));
    }
    let pumpkin = pumpkin_dir();
    let path = Path::new(pumpkin.as_str());
    match DirBuilder::new().mode(0o755).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("making {pumpkin} failed: {error}")),
    }
    // Without following a link, which the image may hold there, to give the user what it leads to.
    let dir = (OpenOptions::new().read(true))
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .context(format_args!("opening {pumpkin} failed"))?;
    fchown(&dir, Some(uid.get()), Some(gid.get()))
        .context(format_args!("giving {pumpkin} to {uid}:{gid} failed"))?;
    sys::become_user(uid.get(), gid.get()).context(format_args!("becoming {uid}:{gid} failed"))?;
    sys::adopt_orphans().context("taking the orphans of sandboxd's commands failed")
}

fn grant() -> Result<RegistrationGrant, String> {
    let Some(grant) = env::var_os(SANDBOXD_GRANT) else {
        return Err(format!(
            "{SANDBOXD_GRANT} is not set: set it to the registration grant issued for the \
             sandbox, which a sandboxd that meets its gateway over TCP presents"
        ));
    };
    // A value that isn't UTF-8 isn't hexadecimal digits either, so it is refused as any other
    // value that isn't a grant's digits.
    let digits = grant.into_string().unwrap_or_default();
    RegistrationGrant::try_from(digits).context(SANDBOXD_GRANT)
}

fn print_line(mut stream: impl Write, line: impl Display) {
    // A failed write is ignored: what `sandboxd` and `sandboxctl` print never changes their
    // outcome, which their exit code reports even when nobody reads their output.
    let _ = writeln!(stream, "{line}");
}
