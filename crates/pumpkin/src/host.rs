//! The controller host's own software. `pumpkin-admin install` installs it as root, from the
//! release that the admin pins.
//!
//! The install makes the user that runs every Pumpkin process there, and makes it linger, so that
//! its job units outlive every SSH session. It writes this `pumpkin-admin` at [`ADMIN`], where the
//! units run it. It writes the host file, which names the controller's settings ([`mod@file`]). It
//! writes the units that reap at boot, stop the controller when idle and copy the members' keys
//! (`members`). It writes `sshd`'s settings, which admit only those keys for the user. Last, it
//! publishes the host's key, which clients pin ([`mod@host_key`]).
//!
//! The host's provisioner installs the OS, Docker and the other packages that jobs run. The
//! install fails where it finds one missing, and names it.
//!
//! The install converges. It writes each file whole, and only where it differs. It says each
//! change, and starts or reloads only what changed. It says nothing when nothing changed, so
//! running it again changes nothing.
// k3s's install keeps a binary whose hash matches, and starts its unit again only if the binary,
// the unit or its environment changed:
// https://github.com/k3s-io/k3s/blob/9e97a3b4aac734441c3ebabe85689303cf35d9a0/install.sh#L559-L581
// https://github.com/k3s-io/k3s/blob/9e97a3b4aac734441c3ebabe85689303cf35d9a0/install.sh#L903-L907
// `teleport install systemd` runs the binary that renders the unit, by default:
// https://github.com/gravitational/teleport/blob/1283425b60ec5f60d509ba4c791183d452923ff7/lib/config/systemd/systemd.go#L76-L84

mod boot;
mod file;
mod gateway_ports;
mod host_key;
mod idle;
mod members;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::{fs, io};

use aws::ParameterArn;
pub(crate) use boot::boot_all;
pub(crate) use file::settings;
use gateway_ports::GATEWAY_PORTS;
pub(crate) use idle::idle;
pub(crate) use members::members;
use stdx::Context as _;
use stdx::process::{Bound, Echo, Environment, Failure, Run};

use crate::cli::Exit;
use crate::controller::USER;
use crate::{say, whole};

/// Where the install puts the `pumpkin-admin` that runs it. Each unit runs that copy.
const ADMIN: &str = "/usr/local/sbin/pumpkin-admin";

// The login shell of `USER`. `sshd` runs each forced command through it. It reads no startup file
// of the user's for `-c`, so no file that a job writes in the home runs before a member's command.
const SHELL: &str = "/bin/sh";

// The group that Docker's package makes. The engine serves its members as root.
const DOCKER_GROUP: &str = "docker";

const UNITS_DIR: &str = "/etc/systemd/system";

const BOOT_UNIT: &str = crate::job::BOOT_UNIT;

const IDLE_TIMER: &str = "pumpkin-idle.timer";

const MEMBERS_TIMER: &str = "pumpkin-members.timer";

// Each unit, by name, as its file holds it before `fill`.
const UNITS: [(&str, &str); 5] = [
    (BOOT_UNIT, include_str!("host/pumpkin-boot.service")),
    (
        "pumpkin-idle.service",
        include_str!("host/pumpkin-idle.service"),
    ),
    (IDLE_TIMER, include_str!("host/pumpkin-idle.timer")),
    (
        "pumpkin-members.service",
        include_str!("host/pumpkin-members.service"),
    ),
    (MEMBERS_TIMER, include_str!("host/pumpkin-members.timer")),
];

// The units that are enabled. The timers start the services that are not.
const ENABLED: [&str; 3] = [BOOT_UNIT, IDLE_TIMER, MEMBERS_TIMER];

// The enabled units that start as soon as they are enabled. The boot unit starts at boot.
const STARTED: [&str; 2] = [IDLE_TIMER, MEMBERS_TIMER];

const SSHD_DIR: &str = "/etc/ssh/sshd_config.d";

// Each of `sshd`'s drop-ins, by name, as its file holds it before `fill`.
const SSHD: [(&str, &str); 2] = [
    (
        "00-pumpkin-keepalive.conf",
        include_str!("host/00-pumpkin-keepalive.conf"),
    ),
    (
        "10-pumpkin-members.conf",
        include_str!("host/10-pumpkin-members.conf"),
    ),
];

// The bound of each program that the install runs for systemd. A `systemctl` that waits for a
// unit's start meets systemd's own bound first, which fails the start after 90 s by default. This
// bound is past it.
const SYSTEM_TIMEOUT: Duration = Duration::from_mins(2);

/// Installs the host's software, as the module says, for the controller whose settings are SSM
/// parameter `settings`. Returns 0.
///
/// # Errors
///
/// Fails if it doesn't run as root, if the host's ephemeral ports aren't the gateway's, if the
/// provisioner's part is missing, or if a step fails. The steps before the failed one stay done.
/// The install copies the members' keys first, so settings that name no members' keys change
/// nothing, and a copy that fails leaves `sshd` as it was.
pub(crate) fn install(settings: &ParameterArn) -> Result<Exit, String> {
    if !rustix::process::geteuid().is_root() {
        return Err(
            "this `pumpkin-admin install` doesn't run as root: run it as root, as with sudo"
                .to_owned(),
        );
    }
    ports()?;
    let deployment = members::deployment(settings)?;
    members::copy(&deployment)?;
    file::write(settings)?;
    user()?;
    let exe = crate::workspace::exe("expected the `pumpkin-admin` to install")?;
    let bytes = fs::read(&exe).context(exe.display())?;
    wrote(Path::new(ADMIN), &bytes, 0o755)?;
    let mut changed = false;
    for (file, text) in UNITS {
        changed |= wrote(
            &Path::new(UNITS_DIR).join(file),
            fill(text).as_bytes(),
            0o644,
        )?;
    }
    units(changed)?;
    self::sshd()?;
    // Last, so that a client that finds this host's key published finds the members admitted.
    host_key::publish(settings.region(), &deployment.name)?;
    Ok(Exit::Success)
}

/// Writes `sshd`'s drop-ins, and reloads `sshd` if any changed. Puts back the drop-ins that it
/// replaced if the reload fails.
fn sshd() -> Result<(), String> {
    let mut written = Vec::new();
    for (file, text) in SSHD {
        let path = Path::new(SSHD_DIR).join(file);
        let before = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context(path.display()),
        };
        if wrote(&path, fill(text).as_bytes(), 0o644)? {
            written.push((path, before));
        }
    }
    if written.is_empty() {
        return Ok(());
    }
    // Ubuntu's `ssh.service` checks the configuration before its reload signals `sshd`. If no
    // `sshd` runs, the next connection starts one, as `ssh.socket` does.
    if let Err(error) = systemctl(&["try-reload-or-restart", "ssh.service"]) {
        // The drop-ins as they were, so that the next start of `sshd` admits no other keys than
        // the running one does, and the next install writes them again.
        for (path, before) in &written {
            match before {
                Some(bytes) => whole::replace(path, bytes, 0o644)?,
                None => fs::remove_file(path).context(path.display())?,
            }
        }
        return Err(format!(
            "{error}: `sshd` runs without the drop-ins that the install wrote, and the install \
             put back the ones it replaced"
        ));
    }
    Ok(())
}

/// The file that holds the host's range of ephemeral ports, which a listen on port 0 picks from.
const PORT_RANGE: &str = "/proc/sys/net/ipv4/ip_local_port_range";

/// Fails unless the host's range of ephemeral ports is within [`GATEWAY_PORTS`].
fn ports() -> Result<(), String> {
    let text = fs::read_to_string(PORT_RANGE).context(PORT_RANGE)?;
    let mut bounds = text.split_whitespace().map(str::parse::<u16>);
    let within = match (bounds.next(), bounds.next(), bounds.next()) {
        (Some(Ok(first)), Some(Ok(last)), None) => {
            GATEWAY_PORTS.contains(&first) && GATEWAY_PORTS.contains(&last)
        }
        _ => false,
    };
    if !within {
        return Err(format!(
            "the host's ephemeral ports, `{}` in {PORT_RANGE}, aren't within {}-{}, which the \
             deployment admits to the gateway from the sandboxes: set them within it, as \
             Linux's default is",
            text.trim(),
            GATEWAY_PORTS.start(),
            GATEWAY_PORTS.end()
        ));
    }
    Ok(())
}

/// `text` with each placeholder replaced by its value. A unit's command line and `sshd`'s
/// configuration read each value as one word.
fn fill(text: &str) -> String {
    (text.replace("@ADMIN@", ADMIN).replace("@USER@", USER)).replace("@MEMBERS@", members::MEMBERS)
}

/// Writes `bytes` to `path` with permission bits `mode`, as [`whole::converge`] does. Says so, and
/// returns true, if it wrote.
fn wrote(path: &Path, bytes: &[u8], mode: u32) -> Result<bool, String> {
    let wrote = whole::converge(path, bytes, mode)?;
    if wrote {
        say(format_args!("wrote {}", path.display()));
    }
    Ok(wrote)
}

/// Makes `dir` if it is missing, and sets its mode to 0755, so that every user may search it
/// whatever root's umask. Says so if it changed the mode.
fn searchable(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).context(dir.display())?;
    let mode = fs::metadata(dir)
        .context(dir.display())?
        .permissions()
        .mode();
    if mode & 0o7777 != 0o755 {
        let set = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        set.context(dir.display())?;
        say(format_args!("made {} 0755", dir.display()));
    }
    Ok(())
}

/// Adds `USER` with its shell and Docker's group, and makes it linger. Skips each step that the
/// user already has.
fn user() -> Result<(), String> {
    let docker = getent("group", DOCKER_GROUP)?.ok_or_else(|| {
        format!(
            "the group `{DOCKER_GROUP}` is missing: install Docker first, whose package makes it"
        )
    })?;
    match getent("passwd", USER)? {
        None => {
            let groups = format!("--groups={DOCKER_GROUP}");
            let shell = format!("--shell={SHELL}");
            let args = ["--create-home", "--user-group", &shell, &groups, USER];
            ran("useradd", &args)?;
            say(format_args!("added the user {USER}"));
        }
        Some(entry) => {
            // name:password:UID:GID:GECOS:home:shell, as passwd(5) says.
            if entry.split(':').nth(6) != Some(SHELL) {
                ran("usermod", &[&format!("--shell={SHELL}"), USER])?;
                say(format_args!("made {SHELL} the shell of {USER}"));
            }
            // name:password:GID:members, the members split by commas, as group(5) says.
            let members = docker.split(':').nth(3).unwrap_or_default();
            if !members.split(',').any(|member| member == USER) {
                ran(
                    "usermod",
                    &["--append", &format!("--groups={DOCKER_GROUP}"), USER],
                )?;
                say(format_args!("added {USER} to the group {DOCKER_GROUP}"));
            }
        }
    }
    // `loginctl enable-linger` makes this file, whose presence is the user's lingering.
    let linger = Path::new("/var/lib/systemd/linger").join(USER);
    if !linger.try_exists().context(linger.display())? {
        ran("loginctl", &["enable-linger", USER])?;
        say(format_args!("made {USER} linger"));
    }
    Ok(())
}

/// The entry that `getent` gives for `key` in `database`, or `None` if it has none.
fn getent(database: &str, key: &str) -> Result<Option<String>, String> {
    match run(command("getent", &[database, key])) {
        Ok(output) => stdx::process::printed(output).map(Some),
        // getent(1) exits with 2 for a key that the database lacks.
        Err(Failure::Exited { output, .. }) if output.status.code() == Some(2) => Ok(None),
        Err(failure) => Err(failure.to_string()),
    }
}

/// Reloads systemd's units if any changed, and enables each unit of `ENABLED` that isn't. Starts
/// the units of `STARTED`, which starts none that already runs.
// A reload re-arms a waiting timer from its new file, so a timer that changed needs no restart:
// https://github.com/systemd/systemd/blob/e8dc52766e1fdb4f8c09c3ab654d1270e1090c8d/src/core/timer.c#L306-L321
fn units(changed: bool) -> Result<(), String> {
    if changed || unloaded()? {
        systemctl(&["daemon-reload"])?;
    }
    let mut disabled = Vec::new();
    // One unit a call, as `is-enabled` exits with 0 if any of those it is given is enabled.
    for unit in ENABLED {
        match run(command("systemctl", &["is-enabled", "--quiet", unit])) {
            Ok(_) => {}
            Err(Failure::Exited { .. }) => disabled.push(unit),
            Err(failure) => return Err(failure.to_string()),
        }
    }
    if !disabled.is_empty() {
        systemctl(&[&["enable"], &disabled[..]].concat())?;
        say(format_args!("enabled {}", disabled.join(" ")));
    }
    systemctl(&[&["start"], &STARTED[..]].concat())
}

/// Whether the file of a unit of `UNITS` has changed since systemd loaded it. It has if an earlier
/// install wrote the files and ended before it reloaded systemd.
// systemd's `NeedDaemonReload` compares a unit's file with what systemd loaded:
// https://github.com/systemd/systemd/blob/e8dc52766e1fdb4f8c09c3ab654d1270e1090c8d/man/org.freedesktop.systemd1.xml#L2413-L2416
fn unloaded() -> Result<bool, String> {
    let mut args = vec!["show", "--property=NeedDaemonReload"];
    args.extend(UNITS.map(|(unit, _)| unit));
    let output = run(command("systemctl", &args)).map_err(|failure| failure.to_string())?;
    let shown = String::from_utf8_lossy(&output.stdout);
    Ok(shown.lines().any(|line| line == "NeedDaemonReload=yes"))
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    ran("systemctl", args)
}

/// Runs `program` with `args` to its end, as [`run`] does. Fails unless it exits with 0.
fn ran(program: &str, args: &[&str]) -> Result<(), String> {
    run(command(program, args))
        .map(drop)
        .map_err(|failure| failure.to_string())
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut command = stdx::process::command(program, Environment::Only(&["PATH"]));
    command.args(args);
    command
}

/// Runs `command` silently within [`SYSTEM_TIMEOUT`], and captures both its outputs.
fn run(mut command: Command) -> Result<std::process::Output, Failure> {
    stdx::process::run(
        &mut command,
        Run::new(Bound::Within(SYSTEM_TIMEOUT), Echo::Silent),
    )
}
