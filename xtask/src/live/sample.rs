//! `cargo xtask sample-driver`: what the driver of a run on the controller holds, as its `/proc`
//! says, read over the SSH that the live suites use.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use stdx::process::{self, Bound, Echo, Failure, Run};

use super::ssh;
use crate::Result;
use crate::flags::SampleDriver;
use crate::repo::STEP_TIMEOUT;
use crate::settings::{Provider, Settings};

/// How often a sample is taken.
const EVERY: Duration = Duration::from_millis(100);

/// How many samples apart each task's own line is printed.
// Once a second, at `EVERY`, rather than each sample, as a driver may run hundreds of tasks. Their
// lines would then be most of the output. A task's switches and `schedstat` are running totals,
// so a second apart still gives each one's rate. The line of the process as a whole, each sample,
// shows a meet's jump.
const TASKS_EVERY: u64 = 10;

/// Samples the driver of run `id` every `EVERY`, until it exits, as [`sampled`] says. The task
/// repository whose `.pumpkin` is `workspace` sent the run to the controller that the
/// configuration names.
pub(crate) fn sample_driver(flags: &SampleDriver) -> Result<()> {
    let SampleDriver { id, workspace } = flags;
    // The id goes into the controller's shell.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(format!("`{id}` is no run's id: give one, as `pumpkin ls` lists it").into());
    }
    let home =
        env::var_os("HOME").ok_or("HOME is unset: set it, as the configuration is below it")?;
    let config = Path::new(&home).join(".config/pumpkin/pumpkin.toml");
    let read = |path: &Path| {
        fs::read_to_string(path)
            .map_err(|error| format!("reading {} failed: {error}", path.display()))
    };
    let settings = Settings::parse(&read(&config)?, &config, Provider::Ec2, None)?;
    let alias = format!("{}.{}", settings.name, settings.region);
    let dir = workspace.join("controller").join(&alias);
    let found: serde_json::Value = serde_json::from_str(&read(&dir.join("found"))?)?;
    let Some(address) = found["address"].as_str() else {
        return Err(format!("the controller's `found` names no address: {found}").into());
    };
    // Each sample's `ssh` shares one connection, which the first opens. The socket's path is
    // relative, as one holds at most 107 bytes.
    let shared = |options: &[&str]| {
        let mut ssh = ssh(Path::new(&home), None, &alias);
        ssh.current_dir(&dir)
            .args(["-o", "ControlPath=sample.sock"])
            .args(options)
            .arg(format!("pumpkin@{address}"));
        ssh
    };
    let remote = |command: &str| {
        let mut ssh = shared(&["-o", "ControlMaster=auto", "-o", "ControlPersist=10"]);
        ssh.arg(command);
        let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent).named("ssh");
        match process::run(&mut ssh, how) {
            // `pgrep` and `grep` exit with 1 for a match they don't find, and `grep` with 2 for a
            // file that it can't read. `ssh` exits with 255 for its own failures.
            Ok(output) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            Err(Failure::Exited { output, .. }) if matches!(output.status.code(), Some(1 | 2)) => {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Err(failure) => Err(failure.to_string()),
        }
    };
    let sampled = sampled(&remote, id);
    // The connection ends with the sampler, as a master may outlive `ControlPersist`.
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent).named("ssh -O exit");
    let _ = process::run(&mut shared(&["-O", "exit"]), how);
    sampled
}

/// Samples the driver of run `id` on the controller that `remote` runs a command on, every
/// `EVERY`, until it exits. Each sample is a line of the driver's `Threads`, `VmRSS` and `VmHWM`.
/// It holds its open files and their soft and hard limits, and its tasks by name. A name counts
/// each run of digits as `#`. Every `TASKS_EVERY`th sample also has a line for each task, with its
/// switches and its `schedstat`.
fn sampled(remote: &dyn Fn(&str) -> std::result::Result<String, String>, id: &str) -> Result<()> {
    let pids = remote(&format!("pgrep --full '/pumpkin driver [0-9]+ {id} '"))?;
    let Some(pid) = pids.split_whitespace().next() else {
        return Err(format!(
            "no driver of run {id} runs on the controller: sample it once the run has started"
        )
        .into());
    };
    let files = format!(
        "grep --with-filename --extended-regexp \
         '^(Threads|VmRSS|VmHWM|Name|voluntary_ctxt_switches|nonvoluntary_ctxt_switches):' \
         /proc/{pid}/status /proc/{pid}/task/*/status; \
         grep --with-filename '' /proc/{pid}/task/*/schedstat; \
         grep --with-filename '^Max open files' /proc/{pid}/limits; \
         printf '%s:\\n' /proc/{pid}/fd/*"
    );
    println!("driver {pid} of run {id}");
    let mut count = 0;
    loop {
        let start = Instant::now();
        let read = remote(&files)?;
        if read.is_empty() {
            println!("the driver has exited");
            return Ok(());
        }
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        print!("{}", sample(&read, at, count % TASKS_EVERY == 0));
        count += 1;
        thread::sleep(EVERY.saturating_sub(start.elapsed()));
    }
}

/// The lines of the sample taken at `at`, since the epoch, of `read`, what `grep` printed of the
/// driver's files. That is `path:key:value`, a `schedstat`'s or a limit's `path:value`, and an
/// open file's `path:`. With `tasks`, a line for each task follows.
fn sample(read: &str, at: Duration, tasks: bool) -> String {
    let (mut process, mut each) = (Vec::new(), BTreeMap::<&str, BTreeMap<&str, &str>>::new());
    let mut files = 0;
    for line in read.lines() {
        let Some((path, rest)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = rest.split_once(':').unwrap_or(("schedstat", rest));
        match path.split('/').collect::<Vec<_>>()[..] {
            ["", "proc", _, "status"] if key == "Threads" || key.starts_with("Vm") => {
                process.push(format!("{key}={}", value.replace([' ', '\t'], "")));
            }
            ["", "proc", _, "task", tid, _] => {
                each.entry(tid).or_default().insert(key, value.trim());
            }
            ["", "proc", _, "limits"] => {
                if let [.., soft, hard, "files"] = rest.split_whitespace().collect::<Vec<_>>()[..] {
                    process.push(format!("open_files_limit={soft}/{hard}"));
                }
            }
            ["", "proc", _, "fd", _] => files += 1,
            _ => {}
        }
    }
    process.push(format!("open_files={files}"));
    let mut named = BTreeMap::<String, usize>::new();
    for task in each.values() {
        let name = task.get("Name").copied().unwrap_or("?");
        let mut grouped = String::new();
        for c in name.chars() {
            if !c.is_ascii_digit() {
                grouped.push(c);
            } else if !grouped.ends_with('#') {
                grouped.push('#');
            }
        }
        *named.entry(grouped).or_default() += 1;
    }
    let at = at.as_secs_f64();
    let mut lines = format!("{at:.3} {}", process.join(" "));
    for (name, count) in &named {
        let _ = write!(lines, " {name}={count}");
    }
    lines.push('\n');
    for (tid, task) in each.iter().filter(|_| tasks) {
        let field = |key| task.get(key).copied().unwrap_or("?");
        let _ = writeln!(
            lines,
            "{at:.3} task {tid} {} voluntary={} involuntary={} schedstat={}",
            field("Name"),
            field("voluntary_ctxt_switches"),
            field("nonvoluntary_ctxt_switches"),
            field("schedstat").replace(' ', ",")
        );
    }
    lines
}
