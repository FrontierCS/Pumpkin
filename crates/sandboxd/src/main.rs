//! `sandboxd`, the supervisor that runs in every sandbox, and, run under the file name
//! `sandboxctl`, the shim through which callers in the sandbox make ops ([`ctl`]).
//!
//! It meets the gateway, at a Unix socket of the sandbox's own, at a TCP address, or with
//! `--listen` where the gateway dials it, opens the session ([`schema::sandbox`]) with its hello,
//! presenting the attestation in [`SANDBOXD_ATTESTATION`] where it dials a TCP address, and serves
//! it until the gateway closes it. With `--tls`, it serves the session under TLS of its own.
//! With `--relay`, it also carries the gateway's connections to its sandbox's companions
//! ([`relay`]).
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

use std::ffi::OsString;
use std::fmt::Display;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, fchown};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use schema::ir::{CompanionName, User};
use schema::sandbox::{
    Attestation, HELLO_TIMEOUT, RegistrationGrant, SANDBOXD_ATTESTATION, SANDBOXD_GIVEN,
    SANDBOXD_GRANT, SANDBOXD_TLS_CERTIFICATE, SANDBOXD_TLS_KEY, TlsIdentity, pumpkin_dir,
};
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
mod relay;
mod session;
mod sys;
mod until;

mod flags {
    #![expect(
        unreachable_pub,
        reason = "xflags declares the types it generates `pub`"
    )]
    #![expect(
        clippy::struct_excessive_bools,
        reason = "xflags generates a field of its own for each switch"
    )]
    #![expect(
        clippy::allow_attributes,
        clippy::allow_attributes_without_reason,
        reason = "xflags generates `#[allow(dead_code)]`, without a reason, on its parsers"
    )]

    use std::net::SocketAddr;

    use super::{Address, Companion, RoleUser};

    xflags::xflags! {
        src "./src/main.rs"

        /// Serves a sandbox's session as its supervisor, until the gateway ends it. Exits with
        /// 0 then, and with 1 on any error, bad arguments included.
        cmd sandboxd {
            /// The gateway's address, over which the session runs: the absolute path of the
            /// sandbox's own Unix socket, or a TCP address, such as `192.0.2.1:7433`. Over TCP,
            /// sandboxd presents the attestation that the environment's SANDBOXD_ATTESTATION
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
            /// With --listen, serves the session under TLS, as a companion behind its sandbox's
            /// relay does. SANDBOXD_TLS_CERTIFICATE and SANDBOXD_TLS_KEY hold the certificate
            /// and its PKCS #8 key, each as the lowercase hexadecimal digits of its DER. No
            /// command that sandboxd starts gets either. Refused without --listen.
            optional --tls
            /// With --listen, relays at this TCP address of the sandbox's own, such as
            /// `0.0.0.0:7435`, from before it listens for the gateway. A connection there first
            /// names a companion, in a line. It is then carried to that companion's sandboxd, or
            /// closed. Needs --relayed. Refused without --listen, and with --tls.
            optional --relay address: SocketAddr
            /// A companion that --relay carries connections to, by its name, which must resolve
            /// within the sandbox. It carries one connection a companion at a time. Repeatable,
            /// once for each companion. Refused without --relay.
            repeated --relayed companion: Companion
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
        pub tls: bool,
        pub relay: Option<SocketAddr>,
        pub relayed: Vec<Companion>,
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
    reason = "sandboxd reports why it stopped on its standard error"
)]
fn main() -> ExitCode {
    stdx::abort_on_panic();
    let mut args = std::env::args_os();
    let name = args.next().unwrap_or_default();
    let args: Vec<OsString> = args.collect();
    if Path::new(&name).file_name() == Some("sandboxctl".as_ref()) {
        return ctl::main(args);
    }
    let given = Given::take();
    let flags::Sandboxd {
        gateway,
        listen,
        by,
        tls,
        relay,
        relayed,
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
    let relaying = relaying(relay, relayed, listen, tls);
    let gateway = relaying.and_then(|relaying| {
        let gateway = gateway_of(gateway, listen, by, tls, given?)?;
        // Before `sandboxd` listens for the gateway, which so finds the relay once it has met it.
        if let Some((address, relayed)) = relaying {
            relay::serve(address, relayed)?;
        }
        Ok(gateway)
    });
    match gateway.and_then(|gateway| session::serve(&gateway, ops, model)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sandboxd: {error}");
            ExitCode::FAILURE
        }
    }
}

fn gateway_of(
    address: Address,
    listen: bool,
    by: Option<u64>,
    tls: bool,
    given: Given,
) -> Result<Gateway, String> {
    let Given {
        attestation,
        grant,
        tls_certificate,
        tls_key,
    } = given;
    let by = by.map(|seconds| Deadline::at_unix(Duration::from_secs(seconds) + UNCLAIMED_SLACK));
    match (address, listen, by) {
        (Address::Socket(_) | Address::Tcp(_), false, _) if tls => Err(
            "--tls serves the session to a gateway that dials sandboxd: give it with --listen"
                .to_owned(),
        ),
        (Address::Socket(socket), false, None) => Ok(Gateway::Socket(socket)),
        (Address::Socket(_), true, _) => Err(
            "--listen takes a TCP address of the sandbox's own: give one, such as `0.0.0.0:7434`, \
             or no --listen with a socket"
                .to_owned(),
        ),
        (Address::Tcp(address), false, None) => {
            parsed_attestation(attestation)
                .map(|attestation| Gateway::Tcp(address, attestation))
        }
        (Address::Tcp(address), true, Some(by)) => {
            let server = tls.then(|| server(tls_certificate, tls_key)).transpose()?;
            parsed_grant(grant).map(|grant| Gateway::Listen(address, grant, by, server))
        }
        (Address::Tcp(_), true, None) => Err(
            "--listen waits until the start's deadline: give it as --by, in seconds since the Unix \
             epoch"
                .to_owned(),
        ),
        (Address::Socket(_) | Address::Tcp(_), false, Some(_)) => Err(
            "--by bounds how long a sandboxd that listens waits: give it with --listen".to_owned(),
        ),
    }
}

/// Where `--relay` says to relay, and the companions that `--relayed` names, if it relays.
fn relaying(
    relay: Option<SocketAddr>,
    relayed: Vec<Companion>,
    listen: bool,
    tls: bool,
) -> Result<Option<(SocketAddr, Vec<CompanionName>)>, String> {
    match (relay, relayed.is_empty()) {
        (None, true) => Ok(None),
        (None, false) => Err(
            "--relayed names a companion that --relay carries to: give it with --relay".to_owned(),
        ),
        (Some(_), true) => Err(
            "--relay carries connections to the companions that --relayed names: name each \
             with a --relayed"
                .to_owned(),
        ),
        (Some(_), false) if !listen => Err(
            "--relay carries the connections of a gateway that dials the sandbox: give it with \
             --listen"
                .to_owned(),
        ),
        (Some(_), false) if tls => Err(
            "--relay runs in a sandbox's `main`, and --tls in a companion: give one of them"
                .to_owned(),
        ),
        (Some(address), false) => {
            let relayed = relayed.into_iter().map(|Companion(name)| name);
            Ok(Some((address, relayed.collect())))
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

/// A companion that `--relayed` names.
#[derive(Debug)]
struct Companion(CompanionName);

impl FromStr for Companion {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        CompanionName::try_from(name.to_owned()).map(Self)
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

/// What a provider gives `sandboxd` alone ([`SANDBOXD_GIVEN`]), each `None` where it is unset.
struct Given {
    attestation: Option<OsString>,
    grant: Option<OsString>,
    tls_certificate: Option<OsString>,
    tls_key: Option<OsString>,
}

impl Given {
    /// Takes what a provider gives `sandboxd` alone from its environment. It is to be called
    /// before `sandboxd` starts a thread, as `stdx::env::take` needs.
    fn take() -> Result<Self, String> {
        // Taken as it starts, as Buildkite's agent unsets its configuration and GitHub's runner
        // its inputs, so that nothing it starts inherits them:
        // https://github.com/buildkite/agent/blob/dc696c5ecad164b5aafc678b76bba69eef5111be/clicommand/agent_start.go#L902-L905
        // https://github.com/actions/runner/blob/d1eb12d4b115c301201de27d667284e1a80f2ab5/src/Runner.Listener/CommandSettings.cs#L110-L139
        let taken = stdx::env::take(SANDBOXD_GIVEN);
        // In the list's order.
        let [attestation, grant, tls_certificate, tls_key] =
            taken.context("listing sandboxd's threads in /proc/self/task failed")?;
        Ok(Self {
            attestation,
            grant,
            tls_certificate,
            tls_key,
        })
    }
}

fn parsed_grant(grant: Option<OsString>) -> Result<RegistrationGrant, String> {
    let Some(grant) = grant else {
        return Err(format!(
            "{SANDBOXD_GRANT} is not set: set it to the registration grant issued for the \
             sandbox, which the gateway that dials a sandboxd that listens presents"
        ));
    };
    // A value that isn't UTF-8 isn't hexadecimal digits either, so it is refused as any other
    // value that isn't a grant's digits.
    let digits = grant.into_string().unwrap_or_default();
    RegistrationGrant::try_from(digits).context(SANDBOXD_GRANT)
}

/// The TLS server that shows the identity whose digits `certificate` and `key` hold.
fn server(certificate: Option<OsString>, key: Option<OsString>) -> Result<tls::Server, String> {
    let digits = |digits: Option<OsString>, variable: &str| {
        let Some(digits) = digits else {
            return Err(format!(
                "{variable} is not set: set it to the digits of the identity issued for the \
                 companion, which a sandboxd that listens with --tls serves TLS with"
            ));
        };
        Ok(digits.into_string().unwrap_or_default())
    };
    let certificate = digits(certificate, SANDBOXD_TLS_CERTIFICATE)?;
    let key = digits(key, SANDBOXD_TLS_KEY)?;
    let identity = TlsIdentity::from_digits(&certificate, &key)?;
    tls::Server::new(&[identity.certificate()], identity.key()).map_err(|error| {
        format!(
            "the identity in {SANDBOXD_TLS_CERTIFICATE} and {SANDBOXD_TLS_KEY} can't serve TLS: \
             {error}: expected the certificate and the key issued for the companion"
        )
    })
}

fn parsed_attestation(attestation: Option<OsString>) -> Result<Attestation, String> {
    let Some(attestation) = attestation else {
        return Err(format!(
            "{SANDBOXD_ATTESTATION} is not set: set it to the sandbox's attestation, which a \
             sandboxd that dials its gateway over TCP presents"
        ));
    };
    Attestation::try_from(attestation.into_vec()).context(SANDBOXD_ATTESTATION)
}

fn print_line(mut stream: impl Write, line: impl Display) {
    // A failed write is ignored: what `sandboxd` and `sandboxctl` print never changes their
    // outcome, which their exit code reports even when nobody reads their output.
    let _ = writeln!(stream, "{line}");
}
