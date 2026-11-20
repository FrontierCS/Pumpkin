//! What the tests of more than one of Pumpkin's crates share.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    reason = "a test helper fails the test that calls it"
)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::{fs, io};

use schema::sandbox::{command_stdout, log_dir};
use stdx::Deadline;
use stdx::process::Environment;

pub mod edge;
pub mod http;
pub mod suite;
mod test_dir;

pub use test_dir::TestDir;

/// A complete workflow, in the IR's JSON.
///
/// Its roles are an agent with a model route, the target of its job function `train`, and the
/// target of its final call, in that order. Their images' digests, commands, files, standard
/// input and environment are placeholders, and so are the bodies' files.
pub const WORKFLOW: &str = include_str!("workflow.json");

/// How long a test waits for what only a test that hangs never sees, so no outcome rests on it.
pub const HUNG: Duration = Duration::from_secs(60);

/// A request's deadline, [`HUNG`] from now.
pub fn hung() -> Deadline {
    Deadline::after(HUNG)
}

/// The files and empty directories below `dir`, by their paths from it, sorted, each executable
/// file marked `*`.
pub fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut lines = Vec::new();
    for name in names {
        let path = dir.join(&name);
        let metadata = fs::metadata(&path).unwrap();
        if metadata.is_dir() {
            let below = files(&path);
            if below.is_empty() {
                lines.push(format!("{name}/"));
            }
            lines.extend(below.iter().map(|below| format!("{name}/{below}")));
        } else if metadata.permissions().mode() & 0o100 != 0 {
            lines.push(format!("{name}*"));
        } else {
            lines.push(name);
        }
    }
    lines
}

/// A small public image, pinned by the digest of its index, that the tests' images build on.
pub const BUSYBOX: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The name of the Docker context the tests reach: `PUMPKIN_DOCKER_CONTEXT`, or `default` if it
/// is unset.
pub fn docker_context() -> String {
    std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned())
}

/// `docker`, given only the environment it needs, against [`docker_context`] alone.
///
/// Tests run `docker` directly only to observe containers and images, or to make and remove their
/// own.
pub fn docker() -> Command {
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut docker = stdx::process::command("docker", kept);
    docker.arg(format!("--context={}", docker_context()));
    docker
}

/// Builds `dockerfile` with [`docker`] into an image tagged `tag`, in `context`, a directory, or
/// in none, given each of `contexts`, a name and an image reference, as a named context, and
/// returns the image's ID.
pub fn build(
    dockerfile: &str,
    context: Option<&Path>,
    contexts: &[(&str, &str)],
    tag: &str,
) -> String {
    let contexts = contexts
        .iter()
        .map(|(name, image)| format!("--build-context={name}=docker-image://{image}"));
    // Without provenance: an attestation gives each build an ID of its own, and as a build moves
    // its tag, the image store drops the image by the tag's old ID, which another test process
    // may be running.
    let mut build = docker();
    build
        .args(["build", "--quiet", "--provenance=false"])
        .args(contexts)
        .args(["--tag", tag]);
    match context {
        Some(context) => build.args(["--file", "-"]).arg(context),
        None => build.arg("-"),
    };
    let mut build = build
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Dropping its standard input once written ends the Dockerfile.
    (build.stdin.take().unwrap())
        .write_all(dockerfile.as_bytes())
        .unwrap();
    let build = build.wait_with_output().unwrap();
    assert!(build.status.success(), "{build:?}");
    String::from_utf8(build.stdout).unwrap().trim().to_owned()
}

/// The static `sandboxd` that `PUMPKIN_SANDBOXD` names, by its canonical path.
pub fn sandboxd() -> PathBuf {
    let Some(sandboxd) = std::env::var_os("PUMPKIN_SANDBOXD") else {
        panic!(
            "`PUMPKIN_SANDBOXD` is unset: run the ignored tests with `cargo xtask live docker`, \
             which builds sandboxd and sets it"
        );
    };
    PathBuf::from(sandboxd).canonicalize().unwrap()
}

/// The standard outputs that `record`, a run's record, holds, each after a line that says whose
/// it is.
///
/// The agent's command's, then each call's commands', in the order of the jobs' numbers and then
/// of the commands'; not a lease's commands', whose log entries hold their ends.
///
/// # Panics
///
/// Panics if a job's directory isn't named by its number, or an output can't be read as UTF-8.
pub fn standard_outputs(record: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    let mut shown = |whose: String, stdout: PathBuf| {
        let Ok(stdout) = fs::read_to_string(stdout) else {
            return false;
        };
        lines.push(format!("{whose} standard output:"));
        lines.extend(stdout.lines().map(str::to_owned));
        true
    };
    let agent = command_stdout();
    let agent = Path::new(agent.as_str());
    for (whose, dir, kind) in sandbox_dirs(record) {
        let dir = record.join(dir);
        match kind {
            Sandbox::Agent => {
                shown(whose, dir.join(agent));
            }
            // A call's body runs each of its commands with its output in `log/<c>`, its `c`th,
            // under the names the agent's command's has in `log`.
            Sandbox::Job => {
                let name = agent.file_name().unwrap();
                let commands = dir.join(log_dir().as_str());
                for command in 0.. {
                    let stdout = commands.join(command.to_string()).join(name);
                    if !shown(format!("{whose} command {command}'s"), stdout) {
                        break;
                    }
                }
            }
            Sandbox::Lease => {}
        }
    }
    lines
}

/// The kind of sandbox that a directory of a run's record is of.
#[derive(Clone, Copy)]
enum Sandbox {
    Agent,
    Job,
    Lease,
}

/// What `record`, a run's record, keeps of each sandbox, the agent's, each call's, each lease's.
///
/// Its `sandbox.json`, on one line, after whose it is.
///
/// # Panics
///
/// Panics if a job's directory isn't named by its number, or a `sandbox.json` can't be read as
/// JSON.
pub fn sandboxes(record: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    for (whose, dir, _) in sandbox_dirs(record) {
        let sandbox = fs::read(record.join(&dir).join("sandbox.json"));
        match sandbox {
            Ok(sandbox) => {
                let sandbox: serde_json::Value = serde_json::from_slice(&sandbox).unwrap();
                lines.push(format!("{whose} sandbox, {dir}/sandbox.json: {sandbox}"));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("{dir}/sandbox.json can't be read: {error}"),
        }
    }
    lines
}

/// Who each sandbox's directory in `record`, a run's record, is of, and the directory, below
/// `record`: the agent's, then each job's, then each lease's, each in the order of their numbers.
fn sandbox_dirs(record: &Path) -> impl Iterator<Item = (String, String, Sandbox)> {
    let numbered = |kind: &'static str, whose: &'static str, sandbox: Sandbox| {
        let entries = fs::read_dir(record.join(kind)).into_iter().flatten();
        let number = |entry: fs::DirEntry| entry.file_name().into_string().unwrap().parse();
        let mut numbers: Vec<u64> = entries
            .map(|entry| number(entry.unwrap()).unwrap())
            .collect();
        numbers.sort_unstable();
        let dir = move |n| (format!("{whose} {n}'s"), format!("{kind}/{n}"), sandbox);
        numbers.into_iter().map(dir)
    };
    let mut agents: Vec<_> = fs::read_dir(record.join("agents"))
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    agents.sort_unstable();
    agents
        .into_iter()
        .map(|role| {
            (
                format!("agent `{role}`'s"),
                format!("agents/{role}"),
                Sandbox::Agent,
            )
        })
        .chain(numbered("jobs", "job", Sandbox::Job))
        .chain(numbered("leases", "lease", Sandbox::Lease))
}
