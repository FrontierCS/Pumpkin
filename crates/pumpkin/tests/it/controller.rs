use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File, Permissions};
use std::io::{self, BufRead as _, BufReader, Read};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use expect_test::{Expect, expect};
use rustix::fs::{OFlags, fcntl_getfl};
use schema::ir::Sha256Digest;
use serde_json::json;
use stdx::process::Environment;

use super::view::Served;
use super::{
    PUMPKIN, Prices, Scratch, asked, cut, ended, install, invocation, lease_places, lock_driver,
    masked_members, poll, report, returned, whole_log, workflow, write_record, write_run,
};

const SENT_MODAL: &str = r#"[modal]
profile = "alice"
classes = { a100 = { gpu = "A100-80GB", gpu_model = "NVIDIA A100-SXM4-80GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 } }
"#;

fn id_ago(days: u64, random: &str) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let then = now.checked_sub(Duration::from_hours(days * 24)).unwrap();
    let stamp = jiff::Timestamp::from_second(i64::try_from(then.as_secs()).unwrap()).unwrap();
    format!("{}-{random}", stamp.strftime("%Y%m%d-%H%M%S"))
}

fn image(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

const EMPTY_TREE: &str = "dff3cfced0b99bbac128797b268e1756972cb1285a305e2dc1b9d96e97e6eb5e";

fn controllers(scratch: &Scratch) -> PathBuf {
    scratch
        .0
        .join(format!("home/.pumpkin/{PINNED}/bin/pumpkin"))
}

const UPSTREAMS: &str = r#"[upstreams.gateway]
url = "https://gateway.example/v1"
key_file = "gateway.key"
models = [{ name = "model-a", upstream_name = "provider/model-a" }]
"#;

fn noted(scratch: &Scratch) -> Vec<String> {
    let path = scratch.0.join("bin/calls");
    let calls = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(path);
    let calls =
        (calls.split_terminator("\n\n")).map(|call| call.lines().collect::<Vec<_>>().join(" "));
    calls.collect()
}

fn check(scratch: &Scratch, text: &str, shown: &[(&str, &str)], expect: &Expect) {
    quiet(scratch);
    let mut text = text.replace(scratch.0.to_str().unwrap(), "<scratch>");
    text = text.replace(EMPTY_TREE, "<empty tree>");
    for (value, name) in shown {
        text = text.replace(value, name);
    }
    // The uptime and the disk that the controller's health reports vary, as does the size of the
    // fixture's files, which an upload shows in KiB, unlike a test's own file or a remainder.
    let lines = text.lines().map(|line| match line.split_once("`: up ") {
        Some((before, after)) => {
            let rest = after.split_once(',').map_or("", |(_, rest)| rest);
            let (rest, disk) = rest.split_once(", disk ").unwrap_or((rest, ""));
            let disk = disk
                .split_once("% used")
                .map_or(disk.to_owned(), |(_, rest)| {
                    format!(", disk <n>% used{rest}")
                });
            format!("{before}`: up <t>,{rest}{disk}")
        }
        None => match (line.strip_suffix(" KiB")).and_then(|rest| rest.rsplit_once(": sending ")) {
            Some((before, _)) => format!("{before}: sending <n> KiB"),
            None => line.to_owned(),
        },
    });
    expect.assert_eq(&lines.collect::<Vec<String>>().join("\n"));
}

// What a command on the controller wrote to its standard error, which `serve` writes only as it
// fails; the copy is removed.
fn said_there(scratch: &Scratch) -> String {
    let path = scratch.0.join("bin/stderr");
    let said = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(path);
    said
}

fn quiet(scratch: &Scratch) {
    let said = said_there(scratch);
    assert!(said.is_empty(), "the controller's standard error: {said}");
}

// Refused before the job reads its settings: outside a user's systemd unit, a file below the
// controller's `.pumpkin`, which a job's `.pumpkin` links to, and a tree not the job's own.
#[test]
fn job_refusals() {
    let scratch = Scratch::new();
    install(&scratch, &format!("home/.pumpkin/{PINNED}/bin"));
    scratch.write(
        &format!("home/store/manifests/{EMPTY_TREE}"),
        "pumpkin-tree/1\n",
    );
    let id = id_ago(0, "0b000004");
    let job = |tree: &str| {
        let args = ["controller", "job", &id, "--tree", tree];
        let mut job = scratch.program(
            controllers(&scratch),
            &[&args[..], &["--", "run", "stand-in/task.star"]].concat(),
        );
        job.env("XDG_RUNTIME_DIR", scratch.0.join("run"));
        job
    };
    let mut unset = job(EMPTY_TREE);
    unset.env_remove("XDG_RUNTIME_DIR");
    let mut lines = vec![super::ran(unset, &scratch.0, false)];
    let planted = sha256(b"planted\n");
    scratch.write(&format!("home/store/{planted}"), "planted\n");
    let mut shown = vec![
        (id.clone(), "<id>".to_owned()),
        (planted.clone(), "<blob>".to_owned()),
    ];
    for path in [".pumpkin/runs/planted/record", "stand-in/notes"] {
        let text = format!("pumpkin-tree/1\nf {planted} 8 {path}\n");
        let tree = sha256(text.as_bytes());
        scratch.write(&format!("home/store/manifests/{tree}"), &text);
        lines.push(super::ran(job(&tree), &scratch.0, false));
        shown.push((tree, format!("<tree of {path}>")));
    }
    let made = scratch.0.join("stand-in/notes").exists();
    lines.push(format!("made the file outside a job's tree: {made}"));
    let shown: Vec<(&str, &str)> = (shown.iter())
        .map(|(value, name)| (value.as_str(), name.as_str()))
        .collect();
    check(
        &scratch,
        &lines.join("\n"),
        &shown,
        &expect![[r#"
            $ env --unset=XDG_RUNTIME_DIR pumpkin controller job <id> --tree <empty tree> -- run stand-in/task.star
            exit status: Some(1)
            pumpkin: XDG_RUNTIME_DIR is unset or empty: run the job in a unit of the user's systemd, as the controller target starts it
            $ XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --tree <tree of .pumpkin/runs/planted/record> -- run stand-in/task.star
            exit status: Some(1)
            pumpkin: <scratch>/home/store/manifests/<tree of .pumpkin/runs/planted/record>: the manifest's line `f <blob> 8 .pumpkin/runs/planted/record` is refused: `.pumpkin/runs/planted/record` can't be sent to the controller, where a job's `.pumpkin` is the controller's own workspace: leave it out of the task
            $ XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --tree <tree of stand-in/notes> -- run stand-in/task.star
            exit status: Some(1)
            pumpkin: the job's working directory, <scratch>, isn't its tree, <scratch>/home/trees/<repository>/<id>, which its start makes: run the job there, as its unit does
            made the file outside a job's tree: false"#]],
    );
}

#[test]
fn boot_without_runs() {
    let scratch = Scratch::new();
    install(&scratch, &format!("home/.pumpkin/{PINNED}/bin"));
    let mut boot = scratch.program(controllers(&scratch), &["controller", "boot"]);
    boot.env("XDG_RUNTIME_DIR", scratch.0.join("run"));
    check(
        &scratch,
        &super::ran(boot, &scratch.0, false),
        &[],
        &expect![[r#"
            $ XDG_RUNTIME_DIR=<scratch>/run pumpkin controller boot
            exit status: Some(0)"#]],
    );
}

#[test]
fn boot_installed_builds() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    write_record(&scratch, &id, true, None);
    move_runs(&scratch, "home/.pumpkin/runs");
    // An install whose `pumpkin` fails its boot, beside the started job's.
    let broken = scratch.0.join("home/.pumpkin/0/bin");
    fs::create_dir_all(&broken).unwrap();
    symlink("/bin/false", broken.join("pumpkin")).unwrap();
    // As the boot's unit runs it, from outside every install.
    let status = scratch.admin(&["boot-all"]).status().unwrap();
    let home = scratch.0.join("home");
    let log = fs::read_to_string(home.join("boot.log")).unwrap();
    let recovered = home
        .join(format!(".pumpkin/runs/{id}/outcome.json"))
        .is_file();
    check(
        &scratch,
        &format!("{status}\n{log}recovered: {recovered}"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 1
            pumpkin: run <id> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            <id>
            controller boot failed for <scratch>/home/.pumpkin/0/bin/pumpkin (exit 1)
            recovered: true"#]],
    );
}

const PINNED: &str = "1fe98e9dcf36c2dafe0df654e3899facc7dfa6f8";

const INSTANCE: &str = "i-0c00000000000a11c";

const SHARED: &str = "repo/.pumpkin/controller/alice.us-west-1";

const KEY: &str = "sk-client-stand-in";

// Runs the controller command in the test's home, with cuts before the requested byte on either
// stream. Each before-N action runs just before the Nth connection. A client home with a
// `.member` is a key that the admin lists under that member, so the command runs as the
// controller's forced command runs it. A `refused` file refuses every key, as `sshd` refuses a key
// that is unlisted. As OpenSSH does, it makes its standard error non-blocking, and leaves it so. It
// copies the command's standard error to its own, as `sshd` and `ssh` do, and to `stderr` beside.
// A `link-pid` file gets the pid of the command, which `serve` runs as.
const SSH: &str = r#"#!/bin/sh
dir=${0%/*}
printf '%s\n' "$@" >> "$dir/argv"
dd oflag=nonblock count=0 status=none >&2
while :; do
    case $1 in
        -E) diagnostics=$2; shift 2 ;;
        -o|-F|-i) shift 2 ;;
        *) break ;;
    esac
done
at=$1
shift
command=$*
if [ -f "$HOME/.member" ]; then set -- pumpkin --member "$(cat "$HOME/.member")"; else set --; fi
first=$(printf '%s\n' "$command" | head --lines=1)
printf '%s\n' "$first" >> "$dir/scripts"
calls=$(wc --lines < "$dir/scripts")
if [ -f "$dir/before-$calls" ]; then sh "$dir/before-$calls"; rm "$dir/before-$calls"; fi
if [ -f "$dir/refused" ]; then
    echo "$at: Permission denied (publickey)." >> "${diagnostics:-/dev/stderr}"
    exit 255
fi
mkfifo "$dir/stderr-$$"
tee --append "$dir/stderr" < "$dir/stderr-$$" >&2 &
tee=$!
exec 4>&2 2> "$dir/stderr-$$"
rm "$dir/stderr-$$"
cd "$dir/../home" || exit 1
export HOME="$PWD" XDG_RUNTIME_DIR="$dir/../run"
if mv "$dir/cut-in" "$dir/cut-in-$$" 2>/dev/null; then
    stdbuf --output=0 head --bytes="$(cat "$dir/cut-in-$$")" | /bin/sh -c "$command" "$@"
    exit 1
fi
if mv "$dir/cut-out" "$dir/cut-out-$$" 2>/dev/null; then
    exec 3<&0; mkfifo "$dir/fifo-$$"
    /bin/sh -c "$command" "$@" <&3 > "$dir/fifo-$$" &
    dd bs=1 count="$(cat "$dir/cut-out-$$")" status=none < "$dir/fifo-$$"
    exit 1
fi
# The command runs apart, so that the output ends only once its standard error is copied, as
# `ssh` writes what `sshd` sent before it ends; a test may stop it by its pid.
exec 3<&0
/bin/sh -c "$command" "$@" <&3 3<&- &
served=$!
if [ -f "$dir/link-pid" ]; then echo "$served" > "$dir/link-pid"; fi
exec 2>&4 4>&-
wait "$served" 2>/dev/null
status=$?
wait "$tee"
exit "$status"
"#;

const SYSTEMCTL: &str = "#!/bin/sh\n";

const SYSTEMD_RUN: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${0%/*}/argv\"\n";

fn set_up_client(scratch: &Scratch) -> [(String, String); 2] {
    if !scratch.0.join("bin/aws").exists() {
        scratch.write_executable(
            "bin/aws",
            b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0%/*}/calls\"\nexit 1\n",
        );
    }
    for (name, script) in [
        ("ssh", SSH),
        ("systemctl", SYSTEMCTL),
        ("systemd-run", SYSTEMD_RUN),
    ] {
        scratch.write_executable(&format!("bin/{name}"), script.as_bytes());
    }
    for dir in ["home", "run"] {
        fs::create_dir_all(scratch.0.join(dir)).unwrap();
    }
    install(scratch, &format!("repo/.pumpkin/{PINNED}/bin"));
    let config = format!(
        "[controllers.alice]\nregion = \"us-west-1\"\n\n{}",
        UPSTREAMS.replace("\"gateway.key\"", "\"gateway-key\"")
    );
    scratch.write("client/.config/pumpkin/pumpkin.toml", &config);
    scratch.write("client/.config/pumpkin/gateway-key", KEY);
    scratch.write("client/.member", "alice\n");
    // Not git's own variables, such as the `GIT_DIR` that `git rebase --exec` exports, which
    // would point `git init` at another repository.
    let git = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]))
        .args(["init", "--quiet", "repo"])
        .current_dir(&scratch.0)
        .status();
    assert!(git.unwrap().success());
    scratch.write_task_in("repo", "stand-in", &workflow(&image('0'), &["true"]));
    scratch.write("repo/.pumpkin/.gitignore", "*\n");
    scratch.write("repo/other/data", "another task's\n");
    let path = format!("{}:/usr/bin:/bin", scratch.0.join("bin").display());
    [
        (
            "HOME".to_owned(),
            scratch.0.join("client").display().to_string(),
        ),
        ("PATH".to_owned(), path),
    ]
}

/// A client of [`set_up_client`] whose workspace has found the controller and holds its install.
fn connected_client(scratch: &Scratch) -> [(String, String); 2] {
    let client = set_up_client(scratch);
    connect(scratch, &client);
    served(scratch);
    client
}

fn on_client(scratch: &Scratch, environment: &[(String, String); 2], args: &[&str]) -> Command {
    on_repo(scratch, "repo", environment, args)
}

// The client's command in its repository `repo`, below the scratch directory.
fn on_repo(
    scratch: &Scratch,
    repo: &str,
    environment: &[(String, String); 2],
    args: &[&str],
) -> Command {
    let pumpkin = scratch
        .0
        .join(format!("{repo}/.pumpkin/{PINNED}/bin/pumpkin"));
    let mut command = scratch.program(pumpkin, args);
    command
        .current_dir(scratch.0.join(repo))
        .envs(environment.iter().map(|(name, value)| (name, value)));
    command
}

// A teammate of the client's, `name`, whose key the admin lists under that name: a home of
// their own, `client-<name>`, with the client's configuration, and a copy of the client's
// repository, `repo-<name>`, that has found the same controller.
fn set_up_member(scratch: &Scratch, name: &str) -> [(String, String); 2] {
    let home = format!("client-{name}");
    let config = fs::read_to_string(scratch.0.join("client/.config/pumpkin/pumpkin.toml"));
    scratch.write(
        &format!("{home}/.config/pumpkin/pumpkin.toml"),
        &config.unwrap(),
    );
    scratch.write(&format!("{home}/.config/pumpkin/gateway-key"), KEY);
    scratch.write(&format!("{home}/.member"), &format!("{name}\n"));
    let repo = format!("repo-{name}");
    fs::create_dir_all(scratch.0.join(&repo)).unwrap();
    for entry in fs::read_dir(scratch.0.join("repo")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == ".pumpkin" {
            continue;
        }
        let copied = stdx::process::command("cp", Environment::Only(&["PATH"]))
            .args(["--recursive", "--"])
            .arg(entry.path())
            .arg(scratch.0.join(&repo))
            .status();
        assert!(copied.unwrap().success());
    }
    install(scratch, &format!("{repo}/.pumpkin/{PINNED}/bin"));
    scratch.write(&format!("{repo}/.pumpkin/.gitignore"), "*\n");
    found_at(
        &scratch
            .0
            .join(format!("{repo}/.pumpkin/controller/alice.us-west-1")),
    );
    [
        (
            "HOME".to_owned(),
            scratch.0.join(home).display().to_string(),
        ),
        (
            "PATH".to_owned(),
            format!("{}:/usr/bin:/bin", scratch.0.join("bin").display()),
        ),
    ]
}

fn modes(dir: &Path) -> String {
    let Ok(entries) = fs::read_dir(dir) else {
        return "gone".to_owned();
    };
    let mut files: Vec<String> = entries
        .map(|entry| {
            let entry = entry.unwrap();
            let mode = entry.metadata().unwrap().permissions().mode() & 0o7777;
            format!("{} {mode:o}", entry.file_name().display())
        })
        .collect();
    files.sort();
    files.join(", ")
}

fn mode_and_modes(dir: &Path) -> String {
    let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
    format!("{mode:o}: {}", modes(dir))
}

fn started_job(scratch: &Scratch) -> ([(String, String); 2], String, String) {
    let client = set_up_client(scratch);
    let (id, ran) = start(scratch, &client);
    (client, id, ran)
}

// As a previous command that found the controller left the address, with no AWS call on the
// next warm connection.
fn connect(scratch: &Scratch, client: &[(String, String); 2]) {
    let dir = scratch.0.join(SHARED);
    if dir.join("found").exists() {
        return;
    }
    let first = on_client(scratch, client, &["ls"]).output().unwrap();
    assert!(!first.status.success(), "{first:?}");
    fs::remove_file(dir.join("failed")).unwrap();
    found_at(&dir);
}

// The location of the controller, in the workspace's directory `dir` of it.
fn found_at(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let found = json!({ "instance": INSTANCE, "address": "255.255.255.255" });
    let file = dir.join("found");
    fs::write(&file, found.to_string()).unwrap();
    fs::set_permissions(file, Permissions::from_mode(0o600)).unwrap();
}

fn start(scratch: &Scratch, client: &[(String, String); 2]) -> (String, String) {
    connect(scratch, client);
    start_in(scratch, "repo", client)
}

// As `start`, from the repository `repo`, whose workspace has found the controller.
fn start_in(scratch: &Scratch, repo: &str, client: &[(String, String); 2]) -> (String, String) {
    served(scratch);
    let mut run = on_repo(scratch, repo, client, &["run", "stand-in/task.star"]);
    let invoked = invocation(&run);
    let ran = run.output().unwrap();
    let said = String::from_utf8(ran.stderr.clone()).unwrap();
    let id = said.lines().find_map(|line| {
        let id = line
            .strip_prefix("pumpkin: run ")?
            .split_once(" goes to controller")?
            .0;
        Some(id.to_owned())
    });
    let id = id.unwrap_or_default();
    let mut ran = report(invoked, &ran, &scratch.0, false);
    if !id.is_empty() {
        ran = ran.replace(&id, "<id>");
    }
    (id, ran)
}

// What every client's `run` says of a job it started, which `keys_on_standard_input` shows.
const STARTED: [&str; 6] = [
    "pumpkin: run <id> goes to controller `alice`",
    "pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it",
    "pumpkin: to follow it: pumpkin wait --controller alice <id>",
    "pumpkin: to stop it: pumpkin cancel --controller alice <id>",
    "printed:",
    "  <id>",
];

// A client's `run`, as `start` describes it, without the lines of `STARTED`.
fn sent(ran: &str) -> String {
    let lines = ran.lines().filter(|line| !STARTED.contains(line));
    lines.collect::<Vec<_>>().join("\n")
}

#[test]
fn keys_on_standard_input() {
    let scratch = Scratch::new();
    let (_, id, ran) = started_job(&scratch);
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let sent = fs::read_to_string(scratch.0.join(format!("run/pumpkin/{id}/gateway.key")));
    let held = stdx::process::command("grep", Environment::Only(&["PATH"]))
        .args(["--recursive", "--files-with-matches", "--fixed-strings"])
        .arg(format!("--exclude-dir={PINNED}"))
        .args(["--", KEY])
        .arg(scratch.0.join("repo/.pumpkin"))
        .output()
        .unwrap();
    let held = String::from_utf8(held.stdout).unwrap();
    let lines = [
        ran,
        format!("an argument holds the key: {}", argv.contains(KEY)),
        format!(
            "ssh ran on the controller: {}",
            argv.contains("controller serve")
        ),
        format!(
            "the job's directory holds the key: {}",
            sent.unwrap() == KEY
        ),
        format!(
            "files in the workspace that hold the key: {}",
            if held.is_empty() { "none" } else { &held }
        ),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 6 of the task's files: sending <n> KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait --controller alice <id>
            pumpkin: to stop it: pumpkin cancel --controller alice <id>
            printed:
              <id>
            an argument holds the key: false
            ssh ran on the controller: true
            the job's directory holds the key: true
            files in the workspace that hold the key: none"#]],
    );
}

#[test]
fn start_with_large_arguments_and_model_catalog() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let config = scratch.0.join("client/.config/pumpkin/pumpkin.toml");
    let models = (0..1500)
        .map(|n| format!(r#"{{ name = "model-{n}", upstream_name = "provider/model-{n}" }}"#))
        .collect::<Vec<_>>()
        .join(", ");
    let contents = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        contents.replace("models = [", &format!("models = [{models}, ")),
    )
    .unwrap();
    let key = b"\0binary key\xff\n";
    fs::write(scratch.0.join("client/.config/pumpkin/gateway-key"), key).unwrap();
    let task = scratch.0.join("repo/stand-in/task.star");
    let contents = fs::read_to_string(&task).unwrap();
    fs::write(
        &task,
        contents.replace("workflow(", "TEXT = arg(\"text\", str)\nworkflow("),
    )
    .unwrap();
    connect(&scratch, &client);
    served(&scratch);
    let word = format!("text={}", "x".repeat(schema::frame::MAX_MESSAGE_BYTES));
    let output = on_client(
        &scratch,
        &client,
        &["run", "stand-in/task.star", "--arg", &word],
    )
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8(output.stdout).unwrap();
    let sent = scratch.0.join("run/pumpkin").join(id.trim());
    let config: toml::Table = fs::read_to_string(sent.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        config["upstreams"]["gateway"]["models"]
            .as_array()
            .unwrap()
            .len(),
        1501
    );
    assert_eq!(fs::read(sent.join("gateway.key")).unwrap(), key);
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    assert!(argv.lines().any(|line| line == word));
}

fn start_head(body_bytes: usize, credential_bytes: usize) -> serde_json::Value {
    json!({ "type": "start", "body_bytes": body_bytes, "credential_bytes": credential_bytes })
}

fn start_job(id: &str, modal_bytes: usize) -> serde_json::Value {
    json!({
        "id": id, "repo": "tasks",
        "tree": format!("sha256:{EMPTY_TREE}"), "install": format!("sha256:{EMPTY_TREE}"),
        "words": ["--", "task.star"],
        "sent": { "controller": "alice" }, "key_bytes": [], "modal_bytes": modal_bytes,
    })
}

fn ask_start(scratch: &Scratch, key: &str, path: &str, id: &str, body: &[u8]) -> serde_json::Value {
    let mut request = Vec::new();
    let credential_bytes = body.len();
    let mut start = start_job(id, credential_bytes);
    start["install"] = json!(format!("sha256:{key}"));
    let mut body = body.to_vec();
    serde_json::to_writer(&mut body, &start).unwrap();
    schema::frame::write(&mut request, &start_head(body.len(), credential_bytes), &[]).unwrap();
    for part in body.chunks(schema::frame::MAX_BODY_BYTES) {
        schema::frame::write(&mut request, &(), part).unwrap();
    }
    let serve = scratch
        .0
        .join("home")
        .join(format!(".pumpkin/{key}/bin/pumpkin"));
    let mut process = scratch.program(serve, &["controller", "serve", "--member", "alice"]);
    process
        .env("PATH", path)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("XDG_RUNTIME_DIR", scratch.0.join("run"));
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), &request).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut frames = &output.stdout[..];
    let _hello = schema::frame::read::<serde_json::Value>(&mut frames)
        .unwrap()
        .unwrap();
    let (answer, _) = schema::frame::read::<serde_json::Value>(&mut frames)
        .unwrap()
        .unwrap();
    answer
}

#[test]
fn retry_start() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let key = served(&scratch);
    let home = scratch.0.join("home");
    let id = id_ago(0, "0123abcd");
    let ask = |body: &[u8]| ask_start(&scratch, &key, &client[1].1, &id, body);
    // The store exists, as another tree's upload makes it.
    fs::create_dir_all(home.join("store")).unwrap();
    let absent = ask(b"no tree yet");
    assert_eq!(absent["type"], "refused");
    assert!(absent["message"].as_str().unwrap().contains(EMPTY_TREE));
    scratch.write(
        &format!("home/store/manifests/{EMPTY_TREE}"),
        "pumpkin-tree/1\n",
    );
    let tmpfs = scratch.0.join(format!("run/pumpkin/{id}"));
    let path = scratch.0.join("without-systemd");
    fs::create_dir(&path).unwrap();
    for tool in ["sh", "find", "awk"] {
        symlink(Path::new("/usr/bin").join(tool), path.join(tool)).unwrap();
    }
    let failed = ask_start(&scratch, &key, path.to_str().unwrap(), &id, b"failed");
    assert_eq!(failed["type"], "refused");
    assert!(
        failed["message"]
            .as_str()
            .unwrap()
            .contains("`systemd-run` didn't start")
    );
    for path in [
        &tmpfs,
        &home.join("builds").join(&id),
        &home.join("trees/tasks").join(&id),
    ] {
        assert!(!path.exists(), "{}", path.display());
    }
    let modal = vec![b'm'; schema::frame::MAX_BODY_BYTES + 1];
    assert_eq!(ask(&modal)["type"], "started");
    assert_eq!(ask(b"repeated")["type"], "already_started");
    assert_eq!(fs::read(tmpfs.join("modal.toml")).unwrap(), modal);
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    assert_eq!(
        argv.lines()
            .filter(|line| line.starts_with("--unit="))
            .count(),
        1
    );
    let pumpkin = home.join(format!(".pumpkin/{key}/bin/pumpkin"));
    assert!(argv.lines().any(|line| Path::new(line) == pumpkin));
}

#[test]
fn start_after_a_cut() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let cut = scratch.0.join("bin/cut-out");
    let (_, first) = start(&scratch, &client);
    assert!(first.contains("exit status: Some(0)"), "{first}");
    let mut replies = Vec::new();
    schema::frame::write(&mut replies, &json!({ "type": "serving" }), &[]).unwrap();
    let missing = json!({"type":"missing", "body_bytes":2});
    schema::frame::write(&mut replies, &missing, &[]).unwrap();
    schema::frame::write(&mut replies, &(), b"[]").unwrap();
    fs::write(cut, (replies.len() + 1).to_string()).unwrap();
    let (id, ran) = start(&scratch, &client);
    assert!(ran.contains("exit status: Some(0)"), "{ran}");
    assert!(ran.contains("starting the job again"), "{ran}");
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    assert_eq!(
        argv.lines()
            .filter(|line| *line == format!("--unit=pumpkin-job-{id}"))
            .count(),
        1
    );
    let sent = scratch.0.join(format!("run/pumpkin/{id}/gateway.key"));
    assert_eq!(fs::read_to_string(sent).unwrap(), KEY);
}

// No file of the client's that another user may read, but a public key.
#[test]
fn modes_of_what_the_client_keeps() {
    // The public key's mode is the umask's: this test's, whatever the machine's.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    // The test's own, which `connect` wrote.
    fs::remove_file(scratch.0.join(SHARED).join("found")).unwrap();
    let lines = [
        format!(
            "this machine's key: {}",
            mode_and_modes(&scratch.0.join("client/.config/pumpkin/ssh"))
        ),
        format!(
            "the saved controller location: {}",
            mode_and_modes(&scratch.0.join(SHARED))
        ),
        format!(
            "the jobs' directories in tmpfs: {}",
            mode_and_modes(&scratch.0.join("run/pumpkin")).replace(&id, "<id>")
        ),
        format!(
            "the job's directory in tmpfs: {}",
            mode_and_modes(&scratch.0.join(format!("run/pumpkin/{id}")))
        ),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            this machine's key: 700: key 600, key.pub 644
            the saved controller location: 700: lock 600
            the jobs' directories in tmpfs: 700: <id> 700
            the job's directory in tmpfs: 700: config.toml 600, gateway.key 600"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn target_refusals() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let mut lines = Vec::new();
    let config = scratch.0.join("client/.config/pumpkin/pumpkin.toml");
    let both = format!(
        "{}\n[providers.local.docker]\n",
        fs::read_to_string(&config).unwrap()
    );
    scratch.write("both.toml", &both);
    // A class with a key that Modal's settings lack, a typo of `usd_per_hour`, so that it is
    // refused before anything is sent.
    let misnamed = SENT_MODAL.replace("[modal]", "[controllers.alice.modal]");
    let misnamed = format!(
        "{}\n{}",
        fs::read_to_string(&config).unwrap(),
        misnamed.replace(
            "startup_seconds = 1800",
            "startup_seconds = 1800, usd_per_hr = 4"
        )
    );
    scratch.write("misnamed.toml", &misnamed);
    // Both or neither of `region` and `address`, an address that `ssh` would take as an option,
    // and an address with the choice of an EC2 controller's.
    let named = fs::read_to_string(&config).unwrap();
    let region = "region = \"us-west-1\"";
    let finds = [
        ("twice.toml", format!("{region}\naddress = \"lab:2222\"")),
        ("unfound.toml", String::new()),
        (
            "optional.toml",
            "address = \"-oProxyCommand=true:22\"".to_owned(),
        ),
        (
            "interface.toml",
            "address = \"lab:2222\"\nssh_interface = \"public_ip\"".to_owned(),
        ),
        (
            "modal.toml",
            format!(
                "address = \"lab:2222\"\n{}",
                SENT_MODAL.replace("[modal]", "[controllers.alice.modal]")
            ),
        ),
    ];
    for (file, find) in &finds {
        scratch.write(file, &named.replace(region, find));
    }
    // The single `[controller]` of earlier configurations, and two controllers, of which a command
    // names neither, or one that isn't there.
    let single = named.replace("[controllers.alice]", "[controller]\nname = \"alice\"");
    let single = format!(
        "{single}\n{}",
        SENT_MODAL.replace("[modal]", "[controller.modal]")
    );
    scratch.write("single.toml", &single);
    scratch.write(
        "several.toml",
        &format!("{named}\n[controllers.lab]\naddress = \"lab:2222\"\n"),
    );
    let files = ["../both.toml", "../misnamed.toml", "../single.toml"];
    let finds = finds.iter().map(|(file, _)| format!("../{file}"));
    let several = ["../several.toml".to_owned()];
    for named in (files.into_iter().map(str::to_owned).chain(finds)).chain(several) {
        let named = named.as_str();
        let mut refused = on_client(
            &scratch,
            &client,
            &["run", "stand-in/task.star", "--config", named],
        );
        let invoked = invocation(&refused);
        lines.push(report(
            invoked,
            &refused.output().unwrap(),
            &scratch.0,
            false,
        ));
    }
    let mut unnamed = on_client(
        &scratch,
        &client,
        &[
            "run",
            "stand-in/task.star",
            "--config",
            "../several.toml",
            "--controller",
            "bob",
        ],
    );
    let invoked = invocation(&unnamed);
    lines.push(report(
        invoked,
        &unnamed.output().unwrap(),
        &scratch.0,
        false,
    ));
    let mut runs = on_client(
        &scratch,
        &client,
        &["run", "stand-in/task.star", "--runs", "runs"],
    );
    let invoked = invocation(&runs);
    lines.push(report(invoked, &runs.output().unwrap(), &scratch.0, false));
    // The task's root above the repository that holds it, which alone is sent.
    let (marker, above) = (
        scratch.0.join("repo/PUMPKIN.star"),
        scratch.0.join("PUMPKIN.star"),
    );
    fs::rename(&marker, &above).unwrap();
    // The files the task loads where its labels now find them, so that the root is what is
    // refused.
    for loaded in [
        "lib/train.star",
        "stand-in/submit.star",
        "stand-in/final.star",
    ] {
        let text = fs::read_to_string(scratch.0.join("repo").join(loaded)).unwrap();
        scratch.write(loaded, &text);
    }
    let mut outside = on_client(&scratch, &client, &["run", "stand-in/task.star"]);
    let invoked = invocation(&outside);
    lines.push(report(
        invoked,
        &outside.output().unwrap(),
        &scratch.0,
        false,
    ));
    fs::rename(&above, &marker).unwrap();
    let calls = fs::read_to_string(scratch.0.join("bin/calls")).unwrap_or_default();
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap_or_default();
    lines.push(format!(
        "calls of aws and ssh: {}",
        calls.lines().count() + argv.lines().count()
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../both.toml
            exit status: Some(1)
            pumpkin: ../both.toml: it names `[controllers]` beside `[providers]`: a machine's runs go either to its controllers or to its own providers, so keep `[controllers]`, beside `[upstreams]`, or remove it
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../misnamed.toml
            exit status: Some(1)
            pumpkin: ../misnamed.toml: no table of a configuration takes `controllers.alice.modal.classes.a100.usd_per_hr`: remove it, as `pumpkin help config` says what each table takes
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../single.toml
            exit status: Some(1)
            pumpkin: ../single.toml: a configuration names each controller in `[controllers.<name>]`, not `[controller]`: rename `[controller]` to `[controllers.alice]`, `[controller.modal]` to `[controllers.alice.modal]` and so on, and remove `name`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../twice.toml
            exit status: Some(1)
            pumpkin: ../twice.toml: `[controllers.alice]` names both or neither of `region` and `address`: name the `region` of a controller that deploy/aws made, or the `address` of one elsewhere
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../unfound.toml
            exit status: Some(1)
            pumpkin: ../unfound.toml: `[controllers.alice]` names both or neither of `region` and `address`: name the `region` of a controller that deploy/aws made, or the `address` of one elsewhere
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../optional.toml
            exit status: Some(1)
            pumpkin: ../optional.toml: TOML parse error at line 2, column 11
              |
            2 | address = "-oProxyCommand=true:22"
              |           ^^^^^^^^^^^^^^^^^^^^^^^^
            `-oProxyCommand=true:22` is not a controller's address: give its host and the port of its `sshd`, such as `lab.example.org:2222` or `192.0.2.1:2222`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../interface.toml
            exit status: Some(1)
            pumpkin: ../interface.toml: `[controllers.alice]` names `ssh_interface` beside `address`, but only a controller that deploy/aws made has addresses to choose from: remove `ssh_interface`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../modal.toml
            exit status: Some(1)
            pumpkin: ../modal.toml: `[controllers.alice]` names `modal` beside `address`, but a Modal provider needs a controller that deploy/aws made: remove `[controllers.alice.modal]`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../several.toml
            exit status: Some(1)
            pumpkin: ../several.toml: it names several controllers, `alice`, `lab`: name one with `--controller`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --config ../several.toml --controller bob
            exit status: Some(1)
            pumpkin: ../several.toml: it names no controller `bob`: name one of `alice`, `lab` with `--controller`
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --runs runs
            exit status: Some(1)
            pumpkin: `--runs` names this machine's, but its configuration sends runs to controller `alice`: leave it out
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: stand-in/task.star: the task's root, <scratch>, is above its git repository, <scratch>/repo, which is all that a run on the controller is sent: put the root's `PUMPKIN.star` in the repository
            calls of aws and ssh: 0"#]],
    );
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = <[u8; 32]>::from(sha2::Sha256::digest(bytes));
    Sha256Digest::from(digest).hex().to_owned()
}

fn file_sha256(path: &Path) -> (String, u64) {
    use sha2::Digest as _;
    let mut file = File::open(path).unwrap();
    let mut sha256 = sha2::Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    let mut size_bytes = 0;
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        sha256.update(&buffer[..read]);
        size_bytes += read as u64;
    }
    let digest = <[u8; 32]>::from(sha256.finalize());
    (Sha256Digest::from(digest).hex().to_owned(), size_bytes)
}

fn walked(dir: &Path, skipped: &[&str]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(below) = dirs.pop() {
        for entry in fs::read_dir(dir.join(&below)).unwrap() {
            let entry = entry.unwrap();
            let path = below.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                if !skipped.iter().any(|name| entry.file_name() == *name) {
                    dirs.push(path);
                }
            } else {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}

fn holders(scratch: &Scratch, named: &[(String, &str)]) -> Vec<(String, String)> {
    let repo = scratch.0.join("repo");
    let mut holders: Vec<(String, String)> = (named.iter())
        .map(|(hex, name)| (hex.clone(), (*name).to_owned()))
        .collect();
    holders.push((EMPTY_TREE.to_owned(), "a tree of no files".to_owned()));
    for path in walked(&repo, &[".git", ".pumpkin"]) {
        let at = repo.join(&path);
        let bytes = match fs::read_link(&at) {
            Ok(target) => target.into_os_string().into_vec(),
            Err(_) => fs::read(&at).unwrap(),
        };
        let hex = sha256(&bytes);
        if !named.iter().any(|(named, _)| *named == hex) {
            holders.push((hex, path.display().to_string()));
        }
    }
    holders
}

fn stored(scratch: &Scratch, holders: &[(String, String)]) -> Vec<String> {
    let store = scratch.0.join("home/store");
    let named = |hex: &str| {
        let names: Vec<&str> = (holders.iter())
            .filter(|(held, _)| held == hex)
            .map(|(_, name)| name.as_str())
            .collect();
        if names.is_empty() {
            format!("unnamed {hex}")
        } else {
            names.join(", ")
        }
    };
    let mut lines = Vec::new();
    for path in walked(&store, &["manifests", "incoming"]) {
        let name = path.to_str().unwrap();
        if name == "lock" {
            continue;
        }
        let (hex, copy) = match name.strip_suffix(".x") {
            Some(hex) => (hex, ", to run"),
            None => (name, ""),
        };
        assert_eq!(file_sha256(&store.join(&path)).0, hex, "{name}");
        let metadata = fs::metadata(store.join(&path)).unwrap();
        let mode = metadata.permissions().mode() & 0o7777;
        let linked = match metadata.nlink() {
            1 => "1 link".to_owned(),
            count => format!("{count} links"),
        };
        lines.push(format!("  {}{copy}: {mode:o}, {linked}", named(hex)));
    }
    lines.sort();
    lines.insert(0, "the store:".to_owned());
    let mut manifests = Vec::new();
    for path in walked(&store.join("manifests"), &[]) {
        let name = path.to_str().unwrap().to_owned();
        let text = fs::read(store.join("manifests").join(&path)).unwrap();
        assert_eq!(sha256(&text), name);
        manifests.push(format!("  a manifest: {}", named(&name)));
    }
    manifests.sort();
    lines.extend(manifests);
    let incoming = walked(&store.join("incoming"), &[]);
    if incoming.is_empty() {
        lines.push("  incoming/: none".to_owned());
    }
    for path in incoming {
        let name = path.to_str().unwrap();
        let size_bytes = fs::metadata(store.join("incoming").join(&path))
            .unwrap()
            .len();
        let hex = name.strip_suffix(".part").unwrap();
        lines.push(format!(
            "  incoming/: partial {}, {size_bytes} bytes",
            named(hex)
        ));
    }
    lines
}

fn calls(scratch: &Scratch) -> String {
    let path = scratch.0.join("bin/scripts");
    let text = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(path);
    let calls: Vec<_> = text.lines().collect();
    if calls.is_empty() {
        return "the calls: none".to_owned();
    }
    format!("the calls: {}", calls.join(", "))
}

fn send_calls(scratch: &Scratch) -> String {
    let key = key();
    calls(scratch).replace(&format!("serve=.pumpkin/{key}/bin/pumpkin"), "serve")
}

fn last_tree(scratch: &Scratch) -> String {
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    let at = lines.iter().rposition(|line| *line == "--tree").unwrap();
    lines[at + 1].to_owned()
}

#[test]
fn send_refused_partial() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    // A full-length partial with the wrong digest must be discarded.
    let hex = sha256(b"noted\n");
    scratch.write(&format!("home/store/incoming/{hex}.part"), "Noted\n");
    let (_, refused) = start(&scratch, &client);
    let mut lines = vec![sent(&refused), send_calls(&scratch)];
    let store = scratch.0.join("home/store");
    lines.push(format!(
        "the store holds it: {}; its partial: {}",
        store.join(&hex).exists(),
        store.join(format!("incoming/{hex}.part")).exists()
    ));
    let (_, again) = start(&scratch, &client);
    lines.extend([sent(&again), send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: controller `alice` lacks all 7 of the task's files: sending <n> KiB
            pumpkin: <scratch>/repo/stand-in/notes changed since it was hashed: send the task again
            the calls: serve
            the store holds it: false; its partial: false
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks all 7 of the task's files: sending <n> KiB
            the calls: serve"#]],
    );
}

#[test]
fn send_carries_on() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    fs::write(scratch.0.join("repo/stand-in/big.bin"), vec![42; 3 << 20]).unwrap();
    // End the link in the second frame of the large file, after the first MiB is durable.
    scratch.write("bin/cut-in", &(1536 << 10).to_string());
    let (_, ran) = start(&scratch, &client);
    let holders = holders(&scratch, &[]);
    let mut lines = vec![sent(&ran), send_calls(&scratch)];
    lines.extend(
        stored(&scratch, &holders)
            .into_iter()
            .filter(|line| line.contains("big.bin") || line.contains("incoming")),
    );
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 3.0 MiB
            pumpkin: controller `alice` didn't answer: ended during the upload: starting the job again now, attempt 2 of 7
            pumpkin: controller `alice` lacks all 7 of the task's files: sending 2.0 MiB
            the calls: serve, serve
              stand-in/big.bin: 444, 1 link
              incoming/: none"#]],
    );
}

#[test]
fn cut_during_group_answers() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let (_, warm) = start(&scratch, &client);
    assert!(warm.contains("exit status: Some(0)"), "{warm}");
    send_calls(&scratch);
    let mut blobs = Vec::new();
    for index in 0..9 {
        let contents = format!("small file {index}");
        scratch.write(&format!("repo/stand-in/new-{index}"), &contents);
        blobs.push(json!({
            "digest": format!("sha256:{}", sha256(contents.as_bytes())),
            "size_bytes": contents.len(),
            "held_bytes": 0,
        }));
    }
    let missing = |body: &[u8]| {
        frame_bytes(&json!({"type":"missing","body_bytes":body.len()}), &[])
            + frame_bytes(&serde_json::Value::Null, body)
    };
    let hello = frame_bytes(&json!({"type":"serving"}), &[]);
    // Cut inside the tree query, then inside the second Stored on the next connection.
    let tree = missing(&serde_json::to_vec(&blobs).unwrap());
    let stored = frame_bytes(&json!({"type":"stored"}), &[]);
    scratch.write("bin/cut-out", &(hello + 1));
    scratch.write(
        "bin/before-2",
        &format!(
            "echo {} > '{}/bin/cut-out'",
            hello + tree + stored + 1,
            scratch.0.display()
        ),
    );
    let (_, ran) = start(&scratch, &client);
    check(
        &scratch,
        &[sent(&ran), send_calls(&scratch)].join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` didn't answer: ended within the answer: starting the job again now, attempt 2 of 7
            pumpkin: controller `alice` lacks 9 of the task's 15 files: sending 108 bytes
            pumpkin: controller `alice` didn't answer: ended within the upload's answer: starting the job again now, attempt 2 of 7
            pumpkin: controller `alice` lacks 1 of the task's 15 files: sending 12 bytes
            the calls: serve, serve, serve"#]],
    );
}

#[test]
fn group_drains_after_a_discarded_partial() {
    let scratch = Scratch::new();
    let store = scratch.0.join("home/store");
    fs::create_dir_all(store.join("incoming")).unwrap();
    let bodies = [b"one", b"two", b"end"];
    let blobs = bodies.map(|body| {
        json!({
            "digest": format!("sha256:{}", sha256(body)), "size_bytes":body.len(), "from":0,
        })
    });
    let mut serve = serve_process(&scratch);
    let mut input = serve.stdin.take().unwrap();
    let puts = json!({"type":"puts","blobs":blobs});
    schema::frame::write(&mut input, &puts, &[]).unwrap();
    schema::frame::write(&mut input, &(), bodies[0]).unwrap();
    let (prefix, rest) = bodies[1].split_at(1);
    schema::frame::write(&mut input, &(), prefix).unwrap();
    let partial = store.join(format!("incoming/{}.part", sha256(bodies[1])));
    poll(|| (fs::metadata(&partial).ok()?.len() == 1).then_some(()));
    fs::remove_file(&partial).unwrap();
    fs::create_dir(partial).unwrap();
    schema::frame::write(&mut input, &(), rest).unwrap();
    schema::frame::write(&mut input, &(), bodies[2]).unwrap();
    // Retrying the group reaches the directory in place of its partial, which the store refuses.
    schema::frame::write(&mut input, &puts, &[]).unwrap();
    for body in bodies {
        schema::frame::write(&mut input, &(), body).unwrap();
    }
    schema::frame::write(&mut input, &json!({"type":"jobs"}), &[]).unwrap();
    drop(input);
    let done = serve.wait_with_output().unwrap();
    assert!(done.status.success(), "{done:?}");
    let mut frames = &done.stdout[..];
    let mut kinds = Vec::new();
    while let Some((head, _)) = schema::frame::read::<serde_json::Value>(&mut frames).unwrap() {
        if let Some(kind) = head["type"].as_str() {
            kinds.push(kind.to_owned());
        }
    }
    let stored = bodies.map(|body| store.join(sha256(body)).exists());
    check(
        &scratch,
        &format!("{kinds:?}\nstored: {stored:?}"),
        &[],
        &expect![[r#"
            ["serving", "stored", "offset_changed", "stored", "refused", "jobs"]
            stored: [true, false, false]"#]],
    );
}

#[test]
fn concurrent_uploads() {
    racing_upload(true);
}

#[test]
fn concurrent_upload_resumes_after_cut() {
    racing_upload(false);
}

fn racing_upload(first_finishes: bool) {
    let scratch = Scratch::new();
    let contents = vec![42; schema::frame::MAX_BODY_BYTES];
    let digest = sha256(&contents);
    let manifest = format!("pumpkin-tree/1\nf {digest} {} notes\n", contents.len());
    let mut query = Vec::new();
    let lacks = json!({"type":"lacks", "manifest":format!("sha256:{}", sha256(manifest.as_bytes())), "body_bytes":manifest.len(), "install":false});
    schema::frame::write(&mut query, &lacks, &[]).unwrap();
    schema::frame::write(&mut query, &(), manifest.as_bytes()).unwrap();
    assert!(serve_fed(&scratch, &query).contains(r#""type":"missing""#));

    let mut put = json!({"type":"put", "digest":format!("sha256:{digest}"), "size_bytes":contents.len(), "from":0});
    let mut first = serve_process(&scratch);
    let mut writing = first.stdin.take().unwrap();
    schema::frame::write(&mut writing, &put, &[]).unwrap();
    let (prefix, rest) = contents.split_at(contents.len() / 2);
    schema::frame::write(&mut writing, &(), prefix).unwrap();
    let partial = scratch.0.join(format!("home/store/incoming/{digest}.part"));
    poll(|| (fs::metadata(&partial).ok()?.len() == prefix.len() as u64).then_some(()));

    // This frame exceeds the pipe's capacity. Its writer can finish only if the second
    // serve reads the body while the first still holds the partial.
    let mut second = serve_process(&scratch);
    let mut racing = second.stdin.take().unwrap();
    schema::frame::write(&mut racing, &put, &[]).unwrap();
    schema::frame::write(&mut racing, &(), &contents).unwrap();
    if first_finishes {
        schema::frame::write(&mut writing, &(), rest).unwrap();
    }
    drop(writing);
    let done = first.wait_with_output().unwrap();
    assert!(done.status.success());
    std::io::Write::write_all(&mut racing, &query).unwrap();
    drop(racing);
    let done = second.wait_with_output().unwrap();
    assert!(done.status.success());
    let mut frames = &done.stdout[..];
    let answer = if first_finishes {
        "stored"
    } else {
        "offset_changed"
    };
    for kind in ["serving", answer, "missing"] {
        let (head, _) = schema::frame::read::<serde_json::Value>(&mut frames)
            .unwrap()
            .unwrap();
        assert_eq!(head["type"], kind);
    }
    let ((), body) = schema::frame::read::<()>(&mut frames).unwrap().unwrap();
    let missing: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    if first_finishes {
        assert_eq!(missing, [] as [serde_json::Value; 0]);
    } else {
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0]["held_bytes"], prefix.len());
        put["from"] = missing[0]["held_bytes"].clone();
        let mut resume = Vec::new();
        schema::frame::write(&mut resume, &put, &[]).unwrap();
        schema::frame::write(&mut resume, &(), rest).unwrap();
        assert!(serve_fed(&scratch, &resume).contains(r#""type":"stored""#));
    }
    assert_eq!(
        fs::read(scratch.0.join("home/store").join(digest)).unwrap(),
        contents
    );
    assert!(!partial.exists());
}

#[test]
fn sends_from_a_repository_with_a_native_name() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let repo = scratch.0.join("repo");
    let native = scratch.0.join(OsStr::from_bytes(b"native-\xff\n"));
    fs::rename(&repo, &native).unwrap();
    symlink(&native, &repo).unwrap();
    let (_, ran) = start(&scratch, &client);
    check(
        &scratch,
        &sent(&ran),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks all 6 of the task's files: sending <n> KiB"#]],
    );
}

#[test]
fn refuses_a_non_utf8_entry_below_the_repository() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write_task_in("repo/tasks", "stand-in", &workflow(&image('0'), &["true"]));
    scratch.write("repo/.gitignore", "*\n");
    let tasks = scratch.0.join("repo/tasks");
    let native = scratch.0.join(OsStr::from_bytes(b"repo/tasks-\xff"));
    fs::rename(&tasks, &native).unwrap();
    symlink(&native, &tasks).unwrap();
    let run = on_client(&scratch, &client, &["run", "tasks/stand-in/task.star"]);
    let refused = super::ran(run, &scratch.0, false);
    check(
        &scratch,
        &refused,
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run tasks/stand-in/task.star
        exit status: Some(1)
        pumpkin: tasks/stand-in/task.star: its path below the git repository isn't UTF-8: rename it so it can be sent to the controller"#]],
    );
}

#[test]
fn send_refusals() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    let mut lines = Vec::new();
    for name in [&b"repo/stand-in/a\nb"[..], b"repo/stand-in/\xff"] {
        let path = scratch.0.join(OsStr::from_bytes(name));
        fs::write(&path, "").unwrap();
        let (_, refused) = start(&scratch, &client);
        lines.extend([refused, send_calls(&scratch)]);
        fs::remove_file(path).unwrap();
    }
    // A budget whose model the client's configuration gives no price.
    let mut budgeted = workflow(&image('0'), &["true"]);
    let route = json!([{ "api": "responses", "models": ["model-a"], "budget": { "microusd": 1 } }]);
    budgeted["roles"][0]["runtime"]["agent"]["egress"] = json!({ "model": route });
    scratch.write_task_in("repo", "stand-in", &budgeted);
    let (_, refused) = start(&scratch, &client);
    lines.extend([refused, send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: `stand-in/a\nb` can't be sent to the controller, whose tree names each file by a line: rename it with no newline or tab, and no empty, `.` or `..` component
            the calls: none
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: `stand-in/�` isn't UTF-8, so it can't be sent to the controller, whose tree names each file by a line of text: rename it
            the calls: none
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: <scratch>/client/.config/pumpkin/pumpkin.toml: `agent` has a model budget, but `model-a` has no price for `input`, `cached`, `cache_write` or `output` tokens: price them in its `usd_per_mtok` in the configuration, or remove the budget
            the calls: none"#]],
    );
}

#[test]
fn sends_what_the_task_reaches_whatever_git_ignores() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/.gitignore", "/stand-in/made\n/other/\n");
    scratch.write("repo/stand-in/made", "made by a script\n");
    let (_, ran) = start(&scratch, &client);
    let manifests = scratch.0.join("home/store/manifests");
    let manifest = fs::read_to_string(manifests.join(last_tree(&scratch))).unwrap();
    let paths = manifest
        .lines()
        .skip(1)
        .filter_map(|line| line.rsplit(' ').next());
    let paths: Vec<&str> = paths.collect();
    check(
        &scratch,
        &format!("{}\nsent: {}", sent(&ran), paths.join(" ")),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks all 7 of the task's files: sending <n> KiB
            sent: PUMPKIN.star lib/train.star stand-in/Dockerfile stand-in/final.star stand-in/made stand-in/submit.star stand-in/task.star"#]],
    );
}

#[test]
fn send_changed_file() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    // Shortened once it is hashed, as the first call to the controller begins.
    let notes = scratch.0.join("repo/stand-in/notes");
    scratch.write(
        "bin/before-1",
        &format!("printf no > '{}'\n", notes.display()),
    );
    let (_, changed) = start(&scratch, &client);
    let mut lines = vec![sent(&changed), send_calls(&scratch)];
    let (_, again) = start(&scratch, &client);
    lines.extend([sent(&again), send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(1)
            pumpkin: controller `alice` lacks all 7 of the task's files: sending <n> KiB
            pumpkin: <scratch>/repo/stand-in/notes changed since the task was hashed: send the task again
            the calls: serve
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks all 7 of the task's files: sending <n> KiB
            the calls: serve"#]],
    );
}

#[test]
fn send_evicted_file() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    let (_, first) = start(&scratch, &client);
    assert!(first.contains("exit status: Some(0)"), "{first}");
    send_calls(&scratch);
    let blob = scratch.0.join("home/store").join(sha256(b"noted\n"));
    fs::remove_file(blob).unwrap();
    let mut lines = Vec::new();
    let (_, gone) = start(&scratch, &client);
    lines.extend([sent(&gone), send_calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks 1 of the task's 7 files: sending 6 bytes
            the calls: serve"#]],
    );
}

fn ended_job(scratch: &Scratch, id: &str, log: &str) {
    let job = format!("home/jobs/{id}");
    scratch.write(&format!("{job}/log"), log);
    scratch.write(&format!("{job}/member"), "alice\n");
    scratch.write(&format!("{job}/out"), &format!("{id}\n"));
    let record = format!("home/.pumpkin/runs/{id}");
    let args = ["stand-in/task.star"];
    scratch.write(
        &format!("{record}/record.json"),
        &json!({ "record_version": "pumpkin.record/v1", "args": args }),
    );
    scratch.write(&format!("{record}/driver.pid"), "4242\n");
    scratch.write(&format!("{record}/outcome.json"), &returned(1.0));
}

#[test]
fn cancel_stops_the_unit_once() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0c000006");
    scratch.write(&format!("home/jobs/{id}/log"), "a job still building\n");
    scratch.write(&format!("home/jobs/{id}/member"), "alice\n");
    scratch.write(&format!("run/pumpkin/{id}/held"), "");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let mut lines = Vec::new();
    for _ in 0..2 {
        let cancel = on_client(&scratch, &client, &["cancel", &id])
            .output()
            .unwrap();
        lines.push(format!(
            "{}: {}",
            cancel.status,
            String::from_utf8_lossy(&cancel.stderr).trim()
        ));
    }
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    assert!(scratch.0.join(format!("home/jobs/{id}")).is_dir());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 0: pumpkin: run <id>'s job on controller `alice` is stopped, before the run started
            exit status: 0: pumpkin: run <id>'s job on controller `alice` is stopped, before the run started
            --user stop pumpkin-job-<id>"#]],
    );
}

#[test]
fn cancel_copies_the_record_when_reaping_fails() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0c000008");
    scratch.write(&format!("home/jobs/{id}/log"), "");
    scratch.write(&format!("home/jobs/{id}/member"), "alice\n");
    write_record(&scratch, &id, true, None);
    scratch.write(&format!("runs/{id}/agents/agent/sandbox.json"), "{}");
    move_runs(&scratch, "home/.pumpkin/runs");
    let cancel = on_client(&scratch, &client, &["cancel", &id])
        .output()
        .unwrap();
    assert_eq!(cancel.status.code(), Some(1));
    let said = String::from_utf8_lossy(&cancel.stderr);
    assert!(
        said.contains("sandbox.json names no provider and id"),
        "{said}"
    );
    let copy = scratch
        .0
        .join(format!("repo/.pumpkin/runs/alice/{id}/outcome.json"));
    let outcome: serde_json::Value = serde_json::from_slice(&fs::read(copy).unwrap()).unwrap();
    assert_eq!(outcome["reason"], "its driver ended without an outcome");
    quiet(&scratch);
}

#[test]
fn reap_by_writer() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let made = "home/.pumpkin/runs";
    let own = id_ago(0, "0e000001");
    let other = id_ago(0, "0e000002");
    let old = id_ago(0, "0e000003");
    let lost = |id: &str, args, providers| {
        let record =
            json!({"record_version":"pumpkin.record/v1","pumpkin":super::this_build(),"args":args});
        scratch.write(&format!("{made}/{id}/record.json"), &record);
        scratch.write(&format!("{made}/{id}/driver.pid"), "4242\n");
        let providers = json!({ "providers": providers });
        scratch.write(&format!("{made}/{id}/provider.json"), &providers);
    };
    lost(&own, json!([]), json!({}));
    let future = json!({"pumpkin":{"version":"1.2.3"},"record_version":"future"});
    for (id, record) in [(&other, future), (&old, json!({"record_version":"old"}))] {
        scratch.write(&format!("{made}/{id}/record.json"), &record);
    }
    // Its provider can't be reached, as the configuration that ran it is gone.
    let unreached = id_ago(0, "0e000004");
    let gone = scratch.0.join("gone.toml");
    let aws = json!({ "aws": { "report": { "region": "us-west-1" } } });
    lost(&unreached, json!(["--config", gone]), aws);
    let mut lines = Vec::new();
    for _ in 0..2 {
        let mut reap = on_client(&scratch, &client, &["reap"]);
        let invoked = invocation(&reap);
        let reaped = reap.output().unwrap();
        lines.push(report(invoked, &reaped, &scratch.0, false));
    }
    let outcome = |id: &str| scratch.0.join(format!("{made}/{id}/outcome.json"));
    assert!(outcome(&own).is_file() && !outcome(&other).exists() && !outcome(&old).exists());
    check(
        &scratch,
        &lines.join("\n"),
        &[
            (&own, "<own>"),
            (&other, "<other>"),
            (&old, "<old>"),
            (&unreached, "<unreached>"),
        ],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap
            exit status: Some(3)
            pumpkin: run <own> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <unreached>'s sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/gone.toml, can't be read (No such file or directory (os error 2)): put it back there, as it was when the run ran; once it can be, `pumpkin reap --controller alice` removes them, and if it is gone for good, remove the record, <scratch>/home/.pumpkin/runs/<unreached>, to stop reaping it
            pumpkin: run <unreached> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: to reap runs made by an older pumpkin, use that build: <old>
            pumpkin: to reap runs made by pumpkin 1.2.3, use that build: <other>
            pumpkin: 1 run ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --controller alice
            printed:
              <own>
              <unreached>
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap
            exit status: Some(3)
            pumpkin: run <unreached>'s sandboxes on provider `aws`, if it left any, can't be removed, as it can't be reached: the configuration that ran it, <scratch>/gone.toml, can't be read (No such file or directory (os error 2)): put it back there, as it was when the run ran; once it can be, `pumpkin reap --controller alice` removes them, and if it is gone for good, remove the record, <scratch>/home/.pumpkin/runs/<unreached>, to stop reaping it
            pumpkin: to reap runs made by an older pumpkin, use that build: <old>
            pumpkin: to reap runs made by pumpkin 1.2.3, use that build: <other>
            pumpkin: 1 run ran on a provider that can't be reached, as said above: what each left there may remain
            pumpkin: to reap again once it can be reached: pumpkin reap --controller alice"#]],
    );
}

#[test]
fn reap_after_a_cut() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0e000004");
    write_record(&scratch, &id, true, None);
    move_runs(&scratch, "home/.pumpkin/runs");
    let record = format!("home/.pumpkin/runs/{id}");
    scratch.write(
        &format!("{record}/provider.json"),
        &json!({"providers": {}}),
    );
    // One byte of the reply proves the first sweep finished.
    let cut = frame_bytes(&json!({ "type": "serving" }), &[]) + 1;
    scratch.write("bin/cut-out", &cut);
    let mut reap = on_client(&scratch, &client, &["reap"]);
    let invoked = invocation(&reap);
    let reaped = reap.output().unwrap();
    let outcome = fs::read(scratch.0.join(format!("{record}/outcome.json"))).unwrap();
    let outcome: serde_json::Value = serde_json::from_slice(&outcome).unwrap();
    assert_eq!(outcome["state"], "failed");
    check(
        &scratch,
        &report(invoked, &reaped, &scratch.0, false),
        &[(&id, "<id>")],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin reap
            exit status: Some(0)
            pumpkin: the reap's reply was cut: the output may omit ids of runs already cleaned
            pumpkin: controller `alice` didn't answer: ended within the answer: reaping the runs again now, attempt 2 of 7"#]],
    );
}

fn frame_bytes(message: &serde_json::Value, body: &[u8]) -> usize {
    let mut frame = Vec::new();
    schema::frame::write(&mut frame, message, body).unwrap();
    frame.len()
}

// Status precedes Follow on the same link. The job is the only one in this controller home, and
// is alice's.
fn named_bytes(id: &str) -> usize {
    let jobs = json!({ "member": "alice", "jobs": [{ "id": id, "owner": { "type": "member", "member": "alice" } }] });
    let body = serde_json::to_vec(&jobs).unwrap();
    frame_bytes(&json!({ "type": "jobs", "body_bytes": body.len() }), &[])
        + frame_bytes(&serde_json::Value::Null, &body)
}

#[test]
fn follow_carries_on() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0f000001");
    let log = "building\0task\nstep 1\nstep 1\npublish";
    ended_job(&scratch, &id, log);
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let logged = frame_bytes(&json!({ "type": "logged" }), log.as_bytes());
    let mut lines = Vec::new();
    for (when, bytes) in [
        ("within the log", hello + logged - 1),
        ("after the log", hello + logged),
        ("within the ending", hello + logged + 8),
    ] {
        let _ = calls(&scratch);
        scratch.write(
            "bin/before-1",
            &format!(
                "echo {} > '{}/bin/cut-out'",
                bytes + named_bytes(&id),
                scratch.0.display()
            ),
        );
        let _ = fs::remove_dir_all(scratch.0.join(format!("repo/.pumpkin/runs/alice/{id}")));
        let waited = on_client(&scratch, &client, &["wait", &id])
            .output()
            .unwrap();
        assert!(waited.status.success(), "{waited:?}");
        let stderr = String::from_utf8(waited.stderr).unwrap();
        let (shown, _) = stderr.split_once("pumpkin: copying run").unwrap();
        lines.push(format!("cut {when}: {:?}", shown.replace('\0', "<NUL>")));
    }
    check(
        &scratch,
        &lines.join("\n"),
        &[(id.as_str(), "<id>")],
        &expect![[r#"
            cut within the log: "pumpkin: controller `alice` didn't answer: ended within the answer: following job <id> again now, attempt 2 of 7\nbuilding<NUL>task\nstep 1\nstep 1\npublish\n"
            cut after the log: "building<NUL>task\nstep 1\nstep 1\npublish\npumpkin: controller `alice` didn't answer: ended before the answer: following job <id> again now, attempt 2 of 7\n"
            cut within the ending: "building<NUL>task\nstep 1\nstep 1\npublish\npumpkin: controller `alice` didn't answer: ended within the answer: following job <id> again now, attempt 2 of 7\n""#]],
    );
}

#[test]
fn silent_build_loses_its_follower() {
    let scratch = Scratch::new();
    let id = id_ago(0, "0f000007");
    let log = format!("home/jobs/{id}/log");
    scratch.write(&log, "building\n");
    scratch.write(&format!("home/jobs/{id}/member"), "alice\n");
    let active = scratch.0.join("run/pumpkin").join(&id);
    fs::create_dir_all(&active).unwrap();
    let mut child = scratch
        .program(PUMPKIN, &["controller", "serve", "--member", "alice"])
        .env("XDG_RUNTIME_DIR", scratch.0.join("run"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = json!({ "type": "follow", "id": id, "log_from_bytes": 0, "after_cut": false });
    schema::frame::write(&mut child.stdin.take().unwrap(), &request, &[]).unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut next = || {
        schema::frame::read::<serde_json::Value>(&mut stdout)
            .unwrap()
            .unwrap()
    };
    assert_eq!(next().0, json!({ "type": "serving" }));
    assert_eq!(next().1, b"building\n");
    scratch.write(&log, "building\nready\n");
    assert_eq!(next().1, b"ready\n");
    drop(stdout);
    let exit = child.wait().unwrap();
    check(
        &scratch,
        &format!("{exit}; build still active: {}", active.is_dir()),
        &[],
        &expect!["exit status: 0; build still active: true"],
    );
}

fn said_bytes(message: &str) -> usize {
    frame_bytes(&json!({ "type": "said", "body_bytes": message.len() }), &[])
        + frame_bytes(&serde_json::Value::Null, message.as_bytes())
}

#[test]
fn follow_repeats_no_milestone() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0f000005");
    let [work, agent] = super::started_work(1);
    let dir = write_run(
        &scratch,
        &id,
        asked(&id, 30),
        &[work, agent, super::agent_exited(2)],
        None,
        &lease_places(Prices::Nothing),
    );
    let driver = lock_driver(&dir);
    move_runs(&scratch, "home/.pumpkin/runs");
    scratch.write(&format!("home/jobs/{id}/out"), &id);
    scratch.write(&format!("home/jobs/{id}/member"), "alice\n");
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let first = format!("following run {id}: ending this command does not stop it");
    let _ = calls(&scratch);
    let started = "   0:01 solver started on aws/judge";
    let cut = hello + named_bytes(&id) + said_bytes(&first) + said_bytes(started) + 3;
    let root = scratch.0.display();
    // Cut in the next milestone. The run ends before the next link opens.
    scratch.write(
        "bin/before-1",
        &format!("echo {cut} > '{root}/bin/cut-out'"),
    );
    scratch.write("outcome", &returned(1.0));
    scratch.write(
        "bin/before-2",
        &format!("cp '{root}/outcome' '{root}/home/.pumpkin/runs/{id}/outcome.json'"),
    );
    let child = on_client(&scratch, &client, &["wait", &id])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let outcome = scratch
        .0
        .join(format!("home/.pumpkin/runs/{id}/outcome.json"));
    poll(|| outcome.exists().then_some(()));
    drop(driver);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let shown: Vec<_> = stderr
        .lines()
        .filter(|line| {
            line.contains("ending this command")
                || line.contains(started)
                || line.contains("0:02 the work phase ended")
                || line.contains("attempt 2 of 7")
        })
        .collect();
    check(
        &scratch,
        &shown.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            pumpkin: following run <id>: ending this command does not stop it
            pumpkin:    0:01 solver started on aws/judge
            pumpkin: controller `alice` didn't answer: ended within the answer: following job <id> again now, attempt 2 of 7
            pumpkin:    0:02 the work phase ended"#]],
    );
}

#[test]
fn follow_cut_often() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0f000002");
    ended_job(&scratch, &id, "");
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    let logged = frame_bytes(&json!({ "type": "logged" }), b"line 01\n");
    let _ = calls(&scratch);
    // A new line on each connection advances the byte offset and starts the pauses over.
    for index in 1..=9 {
        let before = if index == 1 { named_bytes(&id) } else { 0 };
        scratch.write(
            &format!("bin/before-{index}"),
            &format!(
                "echo 'line {index:02}' >> '{0}/home/jobs/{id}/log'\necho {1} > '{0}/bin/cut-out'\n",
                scratch.0.display(), hello + before + logged + 2
            ),
        );
    }
    let waited = on_client(&scratch, &client, &["wait", &id])
        .output()
        .unwrap();
    assert!(waited.status.success(), "{waited:?}");
    let stderr = String::from_utf8(waited.stderr).unwrap();
    let (shown, _) = stderr.split_once("pumpkin: copying run").unwrap();
    let lines: Vec<_> = shown
        .lines()
        .filter(|line| line.starts_with("line "))
        .collect();
    let again = shown.matches("attempt 2 of 7").count();
    check(
        &scratch,
        &format!("{}\n{again} immediate retries", lines.join("\n")),
        &[],
        &expect![[r#"
            line 01
            line 02
            line 03
            line 04
            line 05
            line 06
            line 07
            line 08
            line 09
            9 immediate retries"#]],
    );
}

#[test]
fn follow_cut_before_a_line() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0f000003");
    ended_job(&scratch, &id, "line 01 of 01\n");
    let _ = calls(&scratch);
    let cut = scratch.0.join("bin/cut-out");
    let hello = frame_bytes(&json!({ "type": "serving" }), &[]);
    for cut_index in 0..3 {
        let bytes = if cut_index == 0 {
            hello + named_bytes(&id)
        } else {
            hello
        };
        scratch.write(
            &format!("bin/before-{}", 1 + cut_index),
            &format!("echo {bytes} > '{}'\n", cut.display()),
        );
    }
    let mut waiting = on_client(&scratch, &client, &["wait", &id])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(waiting.stderr.take().unwrap());
    let mut shown = Vec::new();
    for line in stderr.lines() {
        let line = line.unwrap();
        // The third attempt is said before its pause, which the test doesn't wait out.
        let third = line.ends_with("attempt 3 of 7");
        shown.push(line);
        if third {
            break;
        }
    }
    waiting.kill().unwrap();
    waiting.wait().unwrap();
    check(
        &scratch,
        &shown.join("\n"),
        &[(id.as_str(), "<id>")],
        &expect![[r#"
            pumpkin: controller `alice` didn't answer: ended before the answer: following job <id> again now, attempt 2 of 7
            pumpkin: controller `alice` didn't answer: ended before the answer: following job <id> again in 5 s, attempt 3 of 7"#]],
    );
}

#[test]
fn follow_copies_another_writers_record() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0f000006");
    ended_job(&scratch, &id, "build output\n");
    let path = scratch
        .0
        .join(format!("home/.pumpkin/runs/{id}/record.json"));
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/alice/{id}"));
    let mut lines = Vec::new();
    for record in [
        json!({ "pumpkin": { "version": "1.2.3" }, "record_version": "another form" }).to_string(),
        "{".to_owned(),
        json!({ "pumpkin": super::this_build(), "record_version": "another form" }).to_string(),
    ] {
        let _ = fs::remove_dir_all(&copy);
        fs::write(&path, &record).unwrap();
        let waited = on_client(&scratch, &client, &["wait", &id])
            .output()
            .unwrap();
        let stderr = String::from_utf8(waited.stderr).unwrap();
        let copied = fs::read_to_string(copy.join("record.json")).ok() == Some(record);
        lines.push(format!(
            "{}; copied the record: {copied}\n  {}",
            waited.status,
            stderr.lines().last().unwrap()
        ));
    }
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 1; copied the record: true
              pumpkin: copied run <id>'s record to <scratch>/repo/.pumpkin/runs/alice/<id>; it was made by pumpkin 1.2.3: read it with that build
            exit status: 1; copied the record: true
              pumpkin: <scratch>/repo/.pumpkin/runs/alice/<id>/record.json: EOF while parsing an object at line 1 column 1
            exit status: 1; copied the record: false
              pumpkin: controller `alice` refused: <scratch>/home/.pumpkin/runs/<id>/record.json holds record_version "another form": expected "pumpkin.record/v1", which this `pumpkin` reads"#]],
    );
}

// As the first `ls` installs it, but as links to the client's files, which write nothing.
fn served(scratch: &Scratch) -> String {
    let key = key();
    let dir = scratch.0.join(format!("home/.pumpkin/{key}/bin"));
    if dir.is_dir() {
        return key;
    }
    fs::create_dir_all(&dir).unwrap();
    for name in BUILT {
        fs::hard_link(client_bin(scratch).join(name), dir.join(name)).unwrap();
    }
    key
}

// What `install` lays out of a build: its `pumpkin` and the formatter.
const BUILT: [&str; 2] = ["pumpkin", "buildifier"];

fn client_bin(scratch: &Scratch) -> PathBuf {
    scratch.0.join(format!("repo/.pumpkin/{PINNED}/bin"))
}

// The SHA-256 and length of each file that `install` links, in name order: of the files it links
// from, the same in each install, so that the large `pumpkin` is hashed once per test process.
fn built() -> &'static [(&'static str, String, u64); 2] {
    static BUILT_SHA256: LazyLock<[(&str, String, u64); 2]> = LazyLock::new(|| {
        let hashed = |name, path: &Path| {
            let (hex, size_bytes) = file_sha256(path);
            (name, hex, size_bytes)
        };
        [
            hashed("buildifier", &super::buildifier()),
            hashed("pumpkin", Path::new(PUMPKIN)),
        ]
    });
    &BUILT_SHA256
}

// The manifest of what `install` lays out, as the client sends it for its install.
fn install_manifest() -> String {
    let mut text = "pumpkin-tree/1\n".to_owned();
    for (name, hex, size_bytes) in built() {
        let _ = writeln!(text, "x {hex} {size_bytes} {name}");
    }
    text
}

fn key() -> String {
    sha256(install_manifest().as_bytes())
}

// Whether each file of the install in `bin` is the client's file of its name.
fn files_of(scratch: &Scratch, bin: &Path) -> String {
    let lines = BUILT.map(|name| {
        let client = fs::read(client_bin(scratch).join(name)).unwrap();
        let other = if fs::read(bin.join(name)).unwrap() == client {
            ""
        } else {
            "other than "
        };
        format!("  {name}: {other}the client's {name}")
    });
    format!("its files:\n{}", lines.join("\n"))
}

/// Moves the records that `write_record` wrote to `to`, below the scratch directory.
fn move_runs(scratch: &Scratch, to: &str) {
    for entry in fs::read_dir(scratch.0.join("runs")).unwrap() {
        let entry = entry.unwrap();
        let dir = scratch.0.join(to).join(entry.file_name());
        fs::create_dir_all(dir.parent().unwrap()).unwrap();
        fs::rename(entry.path(), dir).unwrap();
    }
}

fn days_ago(path: &Path, days: u64) {
    let at = SystemTime::now() - Duration::from_hours(24 * days);
    File::open(path).unwrap().set_modified(at).unwrap();
}

fn ls_ran(scratch: &Scratch, client: &[(String, String); 2], args: &[&str]) -> String {
    let mut ls = on_client(scratch, client, &["ls"]);
    ls.args(args);
    let output = ls.output().unwrap();
    let invoked = format!("$ pumpkin ls {}", args.join(" "));
    report(invoked.trim_end().to_owned(), &output, &scratch.0, false)
}

#[test]
fn ls_answered_there() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let (ended, live, old) = (
        "20261107-120000-1b8a0001",
        "20261107-120100-1b8a0002",
        "20261106-120000-1b8a0003",
    );
    let failed = json!({ "state": "failed", "reason": "r" });
    write_record(&scratch, ended, true, Some(returned(7.0)));
    write_record(&scratch, live, true, None);
    write_record(&scratch, old, true, Some(failed.clone()));
    move_runs(&scratch, "home/.pumpkin/runs");
    let there = scratch.0.join("home/.pumpkin/runs");
    days_ago(&there.join(format!("{old}/outcome.json")), 2);
    let driver = lock_driver(&there.join(live));
    // The jobs: one of a run that has a record, and one of each state yet to start a run.
    let (waiting, building) = ("20261107-130000-2c9b0004", "20261107-130100-2c9b0005");
    let before_slot = "20261107-130101-2c9b0008";
    let unstarted = ["20261107-130200-2c9b0006", "20261106-130300-2c9b0007"];
    for id in [ended, waiting, building, before_slot]
        .iter()
        .chain(&unstarted)
    {
        scratch.write(&format!("home/jobs/{id}/log"), "");
        scratch.write(&format!("home/jobs/{id}/member"), "alice\n");
    }
    for (id, marker, days) in [(waiting, "waiting", 1), (building, "building", 2)] {
        let path = format!("run/pumpkin/{id}/{marker}");
        scratch.write(&path, "");
        days_ago(&scratch.0.join(path), days);
    }
    fs::create_dir_all(scratch.0.join(format!("run/pumpkin/{before_slot}"))).unwrap();
    days_ago(
        &scratch.0.join(format!("home/jobs/{}/log", unstarted[1])),
        2,
    );
    // The copies here: of a run that the controller lacks, and of one that it lists otherwise.
    let lacked = "20261107-110000-0d0c0008";
    write_record(&scratch, lacked, true, Some(returned(3.0)));
    write_record(&scratch, ended, true, Some(failed));
    move_runs(&scratch, "repo/.pumpkin/runs/alice");
    let mut steps = [&[][..], &["20261106"]].map(|args| ls_ran(&scratch, &client, args));
    // The rows whose times are relative to now.
    for varying in ["20261107-1201", "20261107-13", "20261106-13"] {
        steps = steps.map(|ran| masked_members(&ran, varying, "alice ".len()));
    }
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice`: up <t>, 4 jobs, disk <n>% used
            printed:
              20261107-110000-0d0c0008 -     finished          -  -               -  -: score=3 metrics={"solved":3}
              20261107-120000-1b8a0001 alice finished          -  -               -  -: score=7 metrics={"solved":3}
              20261107-120100-1b8a0002 -     publishing      <t>                  -  -
              20261107-130000-2c9b0004 alice waiting         <t>                  -  waiting 24h for a build slot
              20261107-130100-2c9b0005 alice building        <t>                  -  holding a build slot for 48h
              20261107-130101-2c9b0008 alice building        <t>                  -  -
              20261107-130200-2c9b0006 alice unstarted       <t>                  -  -: its job ended before the run started: to see why: pumpkin wait --controller alice 20261107-130200-2c9b0006
              7 runs: 1 waiting, 2 building, 1 running, 2 finished, 1 unstarted; 2 runs that ended over a day ago aren't listed: to list them: pumpkin ls --controller alice 20261106
            $ pumpkin ls 20261106
            exit status: Some(0)
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            printed:
              20261106-120000-1b8a0003 -     failed            -  -               -  -
              20261106-130300-2c9b0007 alice unstarted       <t>                  -  -: its job ended before the run started: to see why: pumpkin wait --controller alice 20261106-130300-2c9b0007
              2 runs: 1 failed, 1 unstarted"#]],
    );
}

// Its `ssh` makes its own standard error non-blocking. A stream left so fails the writes of each
// process that shares it, as under `2>&1`.
#[test]
fn ls_leaves_its_streams_blocking() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    write_record(
        &scratch,
        "20261107-120000-1b8a0001",
        true,
        Some(returned(7.0)),
    );
    move_runs(&scratch, "home/.pumpkin/runs");
    let (mut reader, writer) = io::pipe().unwrap();
    let mut ls = on_client(&scratch, &client, &["ls"]);
    ls.stdout(writer.try_clone().unwrap())
        .stderr(writer.try_clone().unwrap());
    let status = ls.status().unwrap();
    let flags = fcntl_getfl(&writer).unwrap();
    // The command holds copies of the pipe's write end, which would keep the pipe from its end.
    drop((ls, writer));
    let mut printed = String::new();
    reader.read_to_string(&mut printed).unwrap();
    let blocking = !flags.contains(OFlags::NONBLOCK);
    check(
        &scratch,
        &format!("{status}\n{printed}blocking: {blocking}"),
        &[],
        &expect![[r#"
            exit status: 0
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
            1 run: 1 finished
            blocking: true"#]],
    );
}

#[test]
fn ls_installs_its_serve_there() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    // The executable's local name can differ from its name in the install.
    let bin = client_bin(&scratch);
    let renamed = bin.with_file_name("renamed");
    fs::rename(bin.join("pumpkin"), &renamed).unwrap();
    symlink(&renamed, bin.join("pumpkin")).unwrap();
    fs::hard_link(bin.join("buildifier"), renamed.with_file_name("buildifier")).unwrap();
    connect(&scratch, &client);
    write_record(
        &scratch,
        "20261107-120000-1b8a0001",
        true,
        Some(returned(7.0)),
    );
    move_runs(&scratch, "home/.pumpkin/runs");
    let hex = key();
    let install = scratch.0.join(format!("home/.pumpkin/{hex}"));
    let mut steps = Vec::new();
    for first in [true, false] {
        let ls = ls_ran(&scratch, &client, &[]);
        // Only what it installs is at issue, not what it lists. An `ls` that printed nothing is
        // shown whole.
        let said = ls
            .split_once("\nprinted:")
            .map_or(ls.as_str(), |(said, _)| said);
        steps.push(said.to_owned());
        steps.push(calls(&scratch));
        if first {
            // Set it past what the prune of installs keeps, so that the next `serve`'s mark shows.
            days_ago(&install, 15);
        }
    }
    let modified = fs::metadata(&install).unwrap().modified().unwrap();
    let marked = modified.elapsed().unwrap() < Duration::from_hours(24);
    steps.push(format!(
        "the second serve marked the install as used: {marked}"
    ));
    steps.push(format!("the install: {}", modes(&install)));
    steps.push(format!("its bin: {}", modes(&install.join("bin"))));
    steps.push(files_of(&scratch, &install.join("bin")));
    check(
        &scratch,
        &steps.join("\n"),
        &[(&hex, "<this build>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice` has no serve of this pumpkin: installing it there
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            the calls: serve=.pumpkin/<this build>/bin/pumpkin, install=.pumpkin/<this build>, serve=.pumpkin/<this build>/bin/pumpkin
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            the calls: serve=.pumpkin/<this build>/bin/pumpkin
            the second serve marked the install as used: true
            the install: bin 700
            its bin: buildifier 555, pumpkin 555
            its files:
              pumpkin: the client's pumpkin
              buildifier: the client's buildifier"#]],
    );
}

#[test]
fn ls_refusals() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let mut steps = Vec::new();
    // A run of this build that doesn't read, beside one that does.
    write_record(
        &scratch,
        "20261107-120000-1b8a0001",
        true,
        Some(returned(7.0)),
    );
    write_record(&scratch, "20261107-120100-1b8a0002", false, None);
    move_runs(&scratch, "home/.pumpkin/runs");
    steps.push(ls_ran(&scratch, &client, &[]));
    let runs = scratch.0.join("home/.pumpkin/runs");
    fs::rename(&runs, scratch.0.join("aside")).unwrap();
    fs::write(&runs, "").unwrap();
    steps.push(ls_ran(&scratch, &client, &[]));
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: <scratch>/home/.pumpkin/runs/20261107-120100-1b8a0002/driver.pid isn't there: expected the record of a run to have it
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            pumpkin: 1 run can't be listed, as said above
            printed:
              20261107-120000-1b8a0001 finished          -  -               -  -: score=7 metrics={"solved":3}
              1 run: 1 finished
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: controller `alice` refused: <scratch>/home/.pumpkin/runs: Not a directory (os error 20): expected the directory that runs are recorded in, as `--runs` names it"#]],
    );
}

#[test]
fn repair_uses_the_location_found_while_it_waited() {
    repair_while_waiting(
        None,
        &expect![[r#"
            SSH attempts: 3
            AWS calls: 0"#]],
    );
}

#[test]
fn repair_uses_the_failure_at_the_new_location() {
    repair_while_waiting(
        Some("another command could not reach this location"),
        &expect![[r#"
            SSH attempts: 2
            AWS calls: 0"#]],
    );
}

fn repair_while_waiting(failure: Option<&str>, expected: &Expect) {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let dir = scratch.0.join(SHARED);
    let lock = File::open(dir.join("lock")).unwrap();
    lock.lock().unwrap();
    let _ = noted(&scratch);
    let _ = calls(&scratch);
    for call in 1..=2 {
        scratch.write(
            &format!("bin/before-{call}"),
            &format!("echo 0 > '{}/bin/cut-out'\n", scratch.0.display()),
        );
    }
    let mut child = on_client(&scratch, &client, &["ls"])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting for another command") {
        assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
    }
    // A second replacement can reuse the first inode once all readers have closed it.
    for _ in 0..2 {
        fs::copy(dir.join("found"), dir.join("found.new")).unwrap();
        fs::rename(dir.join("found.new"), dir.join("found")).unwrap();
    }
    if let Some(error) = failure {
        fs::write(dir.join("failed"), error).unwrap();
    }
    drop(lock);
    let output = ended(child, stderr, said);
    assert_eq!(output.status.success(), failure.is_none(), "{output:?}");
    if let Some(error) = failure {
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(error),
            "{output:?}"
        );
    }
    let attempts = fs::read_to_string(scratch.0.join("bin/scripts"))
        .unwrap()
        .lines()
        .count();
    check(
        &scratch,
        &format!(
            "SSH attempts: {attempts}\nAWS calls: {}",
            noted(&scratch).len()
        ),
        &[],
        expected,
    );
}

#[test]
fn commands_share_a_recent_failed_repair() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let _ = calls(&scratch);
    let dir = scratch.0.join("bin");
    for call in 1..=2 {
        scratch.write(
            &format!("bin/before-{call}"),
            &format!("echo 0 > '{}/cut-out'\n", dir.display()),
        );
    }
    let first = ls_ran(&scratch, &client, &[]);
    let attempts = fs::read_to_string(dir.join("scripts"))
        .unwrap()
        .lines()
        .count();
    let _ = calls(&scratch);
    let _ = noted(&scratch);
    let second = ls_ran(&scratch, &client, &[]);
    let recent = fs::read_to_string(scratch.0.join(SHARED).join("failed")).unwrap();
    assert!(first.contains(&recent) && second.contains(&recent));
    let lines = format!(
        "first command: {attempts} SSH attempts\nsecond command: {}\nAWS calls after the failure: {}",
        calls(&scratch),
        noted(&scratch).len()
    );
    check(
        &scratch,
        &lines,
        &[],
        &expect![[r#"
        first command: 2 SSH attempts
        second command: the calls: none
        AWS calls after the failure: 0"#]],
    );
}

#[test]
fn wait_names_its_run_there() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    // Two runs' jobs, whose runs never started, with a prefix in common.
    for id in ["20261107-120000-1b8a0001", "20261107-130000-2c9b0002"] {
        scratch.write(&format!("home/jobs/{id}/log"), "");
        scratch.write(&format!("home/jobs/{id}/member"), "alice\n");
    }
    let mut steps = Vec::new();
    for run in ["1b8a", "20261107", "9f"] {
        let output = on_client(&scratch, &client, &["wait", run])
            .output()
            .unwrap();
        steps.push(report(
            format!("$ pumpkin wait {run}"),
            &output,
            &scratch.0,
            false,
        ));
    }
    steps.push(calls(&scratch));
    check(
        &scratch,
        &steps.join("\n"),
        &[(&hex, "<this build>")],
        &expect![[r#"
            $ pumpkin wait 1b8a
            exit status: Some(1)
            pumpkin: run 20261107-120000-1b8a0001's job on controller `alice` ended before the run started, as its log says above: once that is fixed, run the task again as it was sent; to see that log again: pumpkin wait --controller alice 20261107-120000-1b8a0001
            $ pumpkin wait 20261107
            exit status: Some(1)
            pumpkin: `20261107` matches 2 runs: name one by more of its id
              20261107-120000-1b8a0001
              20261107-130000-2c9b0002
            $ pumpkin wait 9f
            exit status: Some(1)
            pumpkin: no run on controller `alice` matches `9f`: name an id, a unique prefix of an id or of its random digits, or a record's path
            the calls: serve=.pumpkin/<this build>/bin/pumpkin, serve=.pumpkin/<this build>/bin/pumpkin, serve=.pumpkin/<this build>/bin/pumpkin"#]],
    );
}

// What `cmd` of a client said, as `$ pumpkin <args>` shows it.
fn ran_by(scratch: &Scratch, mut cmd: Command, args: &[&str]) -> String {
    let output = cmd.output().unwrap();
    report(
        format!("$ pumpkin {}", args.join(" ")),
        &output,
        &scratch.0,
        false,
    )
}

#[test]
fn members_attributed() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    let member = fs::read_to_string(scratch.0.join(format!("home/jobs/{id}/member"))).unwrap();
    let release = fs::read_to_string(scratch.0.join(format!("home/jobs/{id}/release"))).unwrap();
    let envelope = fs::read_to_string(scratch.0.join(format!("home/jobs/{id}/envelope"))).unwrap();
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    let unit = argv.iter().rposition(|line| *line == "--user").unwrap();
    // Ended before its run started, so that its row's time is no clock's.
    fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    let ls = ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &["ls"]),
        &["ls"],
    );
    let lines = [
        format!("the job's member: {member:?}"),
        format!(
            "the job's release is the build's: {}",
            release == format!("{}\n", runtime::BUILD)
        ),
        format!("the envelopes it takes: {envelope:?}"),
        format!("systemd-run {}", argv[unit..].join(" ")),
        ls,
    ];
    let (install, tree) = (key(), last_tree(&scratch));
    let shown = [
        (id.as_str(), "<id>"),
        (&install, "<install>"),
        (&tree, "<tree>"),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &shown,
        &expect![[r#"
            the job's member: "alice\n"
            the job's release is the build's: true
            the envelopes it takes: "^1.1\n"
            systemd-run --user --quiet --expand-environment=no --unit=pumpkin-job-<id> --working-directory=<scratch>/home/trees/repo/<id> --property=ExitType=cgroup --property=ExecStopPost=/usr/bin/rm --recursive --force <scratch>/bin/../run/pumpkin/<id> --property=StandardOutput=append:<scratch>/home/jobs/<id>/out --property=StandardError=append:<scratch>/home/jobs/<id>/log -- <scratch>/home/.pumpkin/<install>/bin/pumpkin controller job <id> --tree <tree> -- run -- stand-in/task.star
            $ pumpkin ls
            exit status: Some(0)
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            printed:
              <id> alice unstarted         -                  -  -: its job ended before the run started: to see why: pumpkin wait --controller alice <id>
              1 run: 1 unstarted"#]],
    );
}

#[test]
fn members_share_uploads() {
    let scratch = Scratch::new();
    let (_, _, ran) = started_job(&scratch);
    let _ = calls(&scratch);
    let bob = set_up_member(&scratch, "bob");
    let (id, by_bob) = start_in(&scratch, "repo-bob", &bob);
    let member = fs::read_to_string(scratch.0.join(format!("home/jobs/{id}/member"))).unwrap();
    let lines = [
        sent(&ran),
        sent(&by_bob),
        send_calls(&scratch),
        format!("the job's member: {member:?}"),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` lacks all 6 of the task's files: sending <n> KiB
            $ HOME=<scratch>/client-bob PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: controller `alice` holds all 6 of the task's files
            the calls: serve
            the job's member: "bob\n""#]],
    );
}

// Records each stop of a unit, as `systemctl` gets it.
const SYSTEMCTL_NOTED: &str = "#!/bin/sh\necho \"$*\" >> \"${0%/*}/units\"\n";

#[test]
fn cancel_scoped() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let (_, random) = id.rsplit_once('-').unwrap();
    let prefix = &random[..4];
    let mut lines = Vec::new();
    for args in [&["cancel", prefix][..], &["cancel", "--any-member", prefix]] {
        let cancel = on_repo(&scratch, "repo-bob", &bob, args);
        lines.push(ran_by(&scratch, cancel, args));
    }
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>"), (prefix, "<prefix>")],
        &expect![[r#"
            $ pumpkin cancel <prefix>
            exit status: Some(1)
            pumpkin: run <id> on controller `alice` is alice's: to stop it: pumpkin cancel --controller alice --any-member <id>
            $ pumpkin cancel --any-member <prefix>
            exit status: Some(0)
            pumpkin: run <id>'s job on controller `alice` is stopped, before the run started
            --user stop pumpkin-job-<id>"#]],
    );
}

#[test]
fn cancel_full_id_refused() {
    let scratch = Scratch::new();
    let (_, id, _) = started_job(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let args = ["cancel", &id];
    let by_bob = ran_by(&scratch, on_repo(&scratch, "repo-bob", &bob, &args), &args);
    // As a build of Bob's that skips its own check asks it.
    let cancel = json!({ "type": "cancel", "id": id, "any_member": false, "modal_bytes": null });
    let mut asked = Vec::new();
    schema::frame::write(&mut asked, &cancel, &[]).unwrap();
    let served = serve_fed_for(&scratch, &["--member", "bob"], &asked);
    let units = scratch.0.join("bin/units").exists();
    let lines = [by_bob, served, format!("a unit stopped: {units}")];
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            $ pumpkin cancel <id>
            exit status: Some(1)
            pumpkin: run <id> on controller `alice` is alice's: to stop it: pumpkin cancel --controller alice --any-member <id>
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"run <id> is alice's: to stop it: pumpkin cancel --any-member <id>","type":"refused"}, and a body of 0 bytes
            a unit stopped: false"#]],
    );
}

// Jobs yet to start a run, each with the member that `member` names, and its tmpfs directory, so
// that a cancel stops its unit.
fn waiting_jobs(scratch: &Scratch, jobs: &[(&str, &str)]) {
    for (id, member) in jobs {
        scratch.write(&format!("home/jobs/{id}/log"), "");
        scratch.write(&format!("home/jobs/{id}/member"), member);
        fs::create_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    }
}

#[test]
fn cancel_prefers_own_run() {
    let scratch = Scratch::new();
    connected_client(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let (alices, bobs) = (id_ago(0, "0b000001"), id_ago(0, "0b000002"));
    waiting_jobs(&scratch, &[(&alices, "alice\n"), (&bobs, "bob\n")]);
    let mut lines = Vec::new();
    for args in [&["cancel", "0b00"][..], &["cancel", "--any-member", "0b00"]] {
        let cancel = on_repo(&scratch, "repo-bob", &bob, args);
        lines.push(ran_by(&scratch, cancel, args));
    }
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&alices, "<alice's>"), (&bobs, "<bob's>")],
        &expect![[r#"
            $ pumpkin cancel 0b00
            exit status: Some(0)
            pumpkin: run <bob's>'s job on controller `alice` is stopped, before the run started
            $ pumpkin cancel --any-member 0b00
            exit status: Some(1)
            pumpkin: `0b00` matches 2 runs: name one by more of its id
              <alice's>
              <bob's>
            --user stop pumpkin-job-<bob's>"#]],
    );
}

// A run whose member can't be read is left out, with what of its log can't be read.
#[test]
fn ls_says_unread_lines_of_a_run_unlisted() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0c000003");
    let log = [super::logged(5, json!({"kind":"work_started"}), json!([]))];
    let failed = json!({ "state": "failed", "reason": "r" });
    let places = super::lease_places(super::Prices::Nothing);
    let asked = super::asked(&id, 90);
    let dir = super::write_run(&scratch, &id, asked, &log, Some(failed), &places);
    let mut written = fs::read_to_string(dir.join("log.jsonl")).unwrap();
    written.push_str("{\"seq\":\n");
    fs::write(dir.join("log.jsonl"), written).unwrap();
    move_runs(&scratch, "home/.pumpkin/runs");
    scratch.write(&format!("home/jobs/{id}/member"), "Bad!\n");
    let ls = ran_by(&scratch, on_client(&scratch, &client, &["ls"]), &["ls"]);
    check(
        &scratch,
        &ls,
        &[(&id, "<id>")],
        &expect![[r#"
        $ pumpkin ls
        exit status: Some(1)
        pumpkin: run <id>: 1 part of its log can't be read by this pumpkin, the first at line 3: EOF while parsing a value, at column 7
        pumpkin: run <id>: <scratch>/home/jobs/<id>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
        pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
        pumpkin: 1 run can't be listed, as said above"#]],
    );
}

#[test]
fn member_unread() {
    let scratch = Scratch::new();
    connected_client(&scratch);
    let bob = set_up_member(&scratch, "bob");
    scratch.write_executable("bin/systemctl", SYSTEMCTL_NOTED.as_bytes());
    let (unread, other) = (id_ago(0, "0c000001"), id_ago(0, "0c000002"));
    waiting_jobs(&scratch, &[(&unread, "Bad!\n"), (&other, "bob\n")]);
    // Ended before its run started, so that its row's time is no clock's.
    fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{other}"))).unwrap();
    let mut lines = vec![ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &["ls"]),
        &["ls"],
    )];
    let args = ["cancel", &unread];
    lines.push(ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &args),
        &args,
    ));
    let cancel =
        json!({ "type": "cancel", "id": unread, "any_member": false, "modal_bytes": null });
    let mut asked = Vec::new();
    schema::frame::write(&mut asked, &cancel, &[]).unwrap();
    lines.push(serve_fed_for(&scratch, &["--member", "bob"], &asked));
    let args = ["cancel", "--any-member", &unread];
    lines.push(ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bob, &args),
        &args,
    ));
    lines.push(fs::read_to_string(scratch.0.join("bin/units")).unwrap());
    check(
        &scratch,
        &lines.join("\n"),
        &[(&unread, "<unread>"), (&other, "<other>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: run <unread>: <scratch>/home/jobs/<unread>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
            pumpkin: controller `alice`: up <t>, 0 jobs, disk <n>% used
            pumpkin: 1 run can't be listed, as said above
            printed:
              <other> bob unstarted         -                  -  -: its job ended before the run started: to see why: pumpkin wait --controller alice <other>
              1 run: 1 unstarted
            $ pumpkin cancel <unread>
            exit status: Some(1)
            pumpkin: run <unread> on controller `alice` names a member that can't be read: <scratch>/home/jobs/<unread>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter: to stop it: pumpkin cancel --controller alice --any-member <unread>
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"run <unread> names a member that can't be read: <scratch>/home/jobs/<unread>/member: `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter: to stop it: pumpkin cancel --any-member <unread>","type":"refused"}, and a body of 0 bytes
            $ pumpkin cancel --any-member <unread>
            exit status: Some(0)
            pumpkin: run <unread>'s job on controller `alice` is stopped, before the run started
            show --property=ActiveState --property=ExecMainStatus --property=ExecMainExitTimestamp pumpkin-boot.service
            --user stop pumpkin-job-<unread>"#]],
    );
}

#[test]
fn serve_refuses_member() {
    let scratch = Scratch::new();
    connected_client(&scratch);
    let bad = set_up_member(&scratch, "bob");
    scratch.write("client-bob/.member", "Bad!\n");
    let ls = ran_by(
        &scratch,
        on_repo(&scratch, "repo-bob", &bad, &["ls"]),
        &["ls"],
    );
    // What follows is the repair's, which finds no controller, as no `aws` answers.
    let (ls, _) = ls.split_once("; SSH at its last private address").unwrap();
    // Each line that `serve` wrote is quoted whole.
    let there = said_there(&scratch);
    assert!(
        there
            .lines()
            .all(|line| ls.contains(&format!("  {line}\n")))
    );
    check(
        &scratch,
        ls,
        &[],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: the link to controller `alice` wrote to its standard error:
              error: invalid value 'Bad!' for '--member <member>': `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
              pumpkin: to read how to use it: pumpkin controller serve --help
            pumpkin: controller `alice` didn't answer: ended before its hello: opening it again now, attempt 2 of 8
            pumpkin: the link to controller `alice` wrote to its standard error:
              error: invalid value 'Bad!' for '--member <member>': `Bad!` is not a member's name: give one of 1 to 32 of a-z, 0-9 and `-`, starting with a letter
              pumpkin: to read how to use it: pumpkin controller serve --help
            pumpkin: controller `alice` didn't answer: ended before its hello"#]],
    );
}

#[test]
fn enrolment_refused() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let _ = fs::remove_file(scratch.0.join("bin/calls"));
    scratch.write("bin/refused", "");
    let ls = ran_by(&scratch, on_client(&scratch, &client, &["ls"]), &["ls"]);
    let key = fs::read_to_string(scratch.0.join("client/.config/pumpkin/ssh/key.pub")).unwrap();
    let aws = scratch.0.join("bin/calls").exists();
    let lines = [ls, format!("aws ran: {aws}")];
    check(
        &scratch,
        &lines.join("\n"),
        &[(key.trim_end(), "<key.pub>")],
        &expect![[r#"
            $ pumpkin ls
            exit status: Some(1)
            pumpkin: controller `alice` refused this machine's key: ask its admin to add this line under your name in deploy/aws's `members`, and to apply: "<key.pub>". If the key is listed already, run the command again in a minute, once the controller has copied the list
            aws ran: false"#]],
    );
}

fn serve_process(scratch: &Scratch) -> Child {
    serve_for(scratch, &["--member", "alice"])
}

// `controller serve` with `args`, such as the `--member` that a forced command gives.
fn serve_for(scratch: &Scratch, args: &[&str]) -> Child {
    let mut serve = scratch.program(PUMPKIN, &[&["controller", "serve"], args].concat());
    serve.env("HOME", scratch.0.join("home"));
    serve.env("XDG_RUNTIME_DIR", scratch.0.join("run"));
    serve
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    serve.spawn().unwrap()
}

/// What `controller serve --member alice` does with `input` on its standard input.
fn serve_fed(scratch: &Scratch, input: &[u8]) -> String {
    serve_fed_for(scratch, &["--member", "alice"], input)
}

/// As [`serve_fed`], of `controller serve` with `args`.
fn serve_fed_for(scratch: &Scratch, args: &[&str], input: &[u8]) -> String {
    let mut child = serve_for(scratch, args);
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input).unwrap();
    let output = child.wait_with_output().unwrap();
    let mut lines = vec![output.status.to_string()];
    let mut out = &output.stdout[..];
    while let Some((message, body)) = schema::frame::read::<serde_json::Value>(&mut out).unwrap() {
        lines.push(format!("  {message}, and a body of {} bytes", body.len()));
    }
    let said = String::from_utf8_lossy(&output.stderr);
    lines.extend(said.lines().map(|line| format!("  said: {line}")));
    lines.join("\n")
}

#[test]
fn grouped_upload_limits() {
    let scratch = Scratch::new();
    let mut steps = Vec::new();
    let puts = |count, from, size_bytes| {
        let blob =
            json!({"digest":format!("sha256:{EMPTY_TREE}"), "from":from, "size_bytes":size_bytes});
        json!({"type":"puts", "blobs": vec![blob; count]})
    };
    for (what, request) in [
        ("too many blobs in a group:", puts(9, 0, 0)),
        ("a large blob in a group:", puts(2, 0, 32769)),
        ("a resumed blob in a group:", puts(2, 1, 2)),
    ] {
        let mut head = Vec::new();
        schema::frame::write(&mut head, &request, &[]).unwrap();
        steps.push(what.to_owned());
        steps.push(serve_fed(&scratch, &head));
    }
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            too many blobs in a group:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a group holds 2 to 8 whole blobs of at most 32768 bytes each: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a large blob in a group:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a group holds 2 to 8 whole blobs of at most 32768 bytes each: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a resumed blob in a group:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a group holds 2 to 8 whole blobs of at most 32768 bytes each: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes"#]],
    );
}

#[test]
fn serve_refusals() {
    let scratch = Scratch::new();
    let mut steps = vec!["no request:".to_owned(), serve_fed(&scratch, b"")];
    let mut past = (u32::try_from(schema::frame::MAX_MESSAGE_BYTES).unwrap() + 1)
        .to_be_bytes()
        .to_vec();
    past.extend(b"{}");
    steps.push("a frame past the cap:".to_owned());
    steps.push(serve_fed(&scratch, &past));
    let mut other = Vec::new();
    schema::frame::write(&mut other, &json!({ "type": "absent_request" }), &[]).unwrap();
    steps.push("a message that is no request:".to_owned());
    steps.push(serve_fed(&scratch, &other));
    for (what, bytes) in [
        ("an incomplete request body:", 1),
        ("a request body past the cap:", 16 * 1024 * 1024 + 1),
    ] {
        let mut head = Vec::new();
        schema::frame::write(&mut head, &start_head(bytes, 0), &[]).unwrap();
        steps.push(what.to_owned());
        steps.push(serve_fed(&scratch, &head));
    }
    let mut job = start_job("once", 1);
    job["key_bytes"] = json!([usize::MAX]);
    let body = serde_json::to_vec(&job).unwrap();
    let mut bad_lengths = Vec::new();
    schema::frame::write(&mut bad_lengths, &start_head(body.len(), 0), &[]).unwrap();
    schema::frame::write(&mut bad_lengths, &(), &body).unwrap();
    steps.push("credential lengths that overflow:".to_owned());
    steps.push(serve_fed(&scratch, &bad_lengths));
    check(
        &scratch,
        &steps.join("\n"),
        &[],
        &expect![[r#"
            no request:
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
            a frame past the cap:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a frame's message is 65537 bytes: it holds at most 65536: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a message that is no request:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"unknown variant `absent_request`, expected one of `copy`, `copied`, `follow`, `health`, `jobs`, `list`, `view`, `start`, `cancel`, `reap`, `lacks`, `put`, `puts` at line 1 column 24: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            an incomplete request body:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"unexpected end of file: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            a request body past the cap:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a request's body holds at most 16 MiB: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes
            credential lengths that overflow:
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"the job gives a different length for its credentials","type":"refused"}, and a body of 0 bytes"#]],
    );
}

#[test]
fn serve_cut_and_malformed_uploads() {
    let scratch = Scratch::new();
    let mut upload = Vec::new();
    let put = json!({"type":"put", "digest":format!("sha256:{}", sha256(b"x")), "from":0, "size_bytes":1});
    schema::frame::write(&mut upload, &put, &[]).unwrap();
    schema::frame::write(&mut upload, &(), b"xx").unwrap();
    let cut = serve_fed(&scratch, &upload[..upload.len() - 1]);
    let oversized = serve_fed(&scratch, &upload);
    check(
        &scratch,
        &format!(
            "a cut within an upload part:\n{cut}\nan upload part past the declared size:\n{oversized}"
        ),
        &[],
        &expect![[r#"
            a cut within an upload part:
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
            an upload part past the declared size:
            exit status: 1
              {"type":"serving"}, and a body of 0 bytes
              {"message":"a part of the request's body holds 2 bytes: expected 1 to 1: expected a request of this pumpkin","type":"refused"}, and a body of 0 bytes"#]],
    );
}

fn record_files(dir: &Path) -> Vec<String> {
    let files = walked(dir, &[]).into_iter().map(|path| {
        let metadata = fs::symlink_metadata(dir.join(&path)).unwrap();
        assert!(metadata.is_file(), "{}", path.display());
        let bytes = fs::read(dir.join(&path)).unwrap();
        format!(
            "{:o} {} {} {} {}",
            metadata.mode() & 0o7777,
            metadata.len(),
            metadata.mtime(),
            sha256(&bytes),
            path.display().to_string().escape_debug()
        )
    });
    files.collect()
}

fn inodes(dir: &Path) -> Vec<(PathBuf, u64)> {
    let inodes = walked(dir, &[]).into_iter().map(|path| {
        let inode = fs::symlink_metadata(dir.join(&path)).unwrap().ino();
        (path, inode)
    });
    inodes.collect()
}

fn copied_record(
    scratch: &Scratch,
    client: &[(String, String); 2],
    args: &[&str],
    copy: &Path,
) -> Vec<String> {
    let before = if copy.exists() {
        inodes(copy)
    } else {
        Vec::new()
    };
    let waited = on_client(scratch, client, args).output().unwrap();
    let mut lines = vec![format!("{}: {}", args[0], waited.status)];
    let said = String::from_utf8_lossy(&waited.stderr);
    let said = said.lines().filter(|line| {
        let ending = line.starts_with("pumpkin: copying run") || line.contains("its record is");
        line.contains("connection") || line.contains("record") && !ending
    });
    lines.extend(said.map(|line| format!("  {line}")));
    lines.push(format!("  {}", calls(scratch)));
    let written = inodes(copy)
        .into_iter()
        .filter(|file| !before.contains(file));
    let written: Vec<String> = written
        .map(|(path, _)| path.display().to_string().escape_debug().to_string())
        .collect();
    lines.push(format!("  written: {}", written.join(", ")));
    lines
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn record_copy() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    let id = id_ago(0, "0d000001");
    ended_job(&scratch, &id, "built\n");
    let record = scratch.0.join(format!("home/.pumpkin/runs/{id}"));
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/alice/{id}"));
    let at = |path: &str| format!("home/.pumpkin/runs/{id}/{path}");
    scratch.write(&at("log.jsonl"), "{\"a\": 1}\n");
    scratch.write(&at("model/a b"), "a name with a space\n");
    scratch.write(&at("model/a\nb"), "a name with a newline\n");
    scratch.write(&at("model/marked"), "set-id and sticky\n");
    scratch.write(&at("jobs/0/out"), "out\n");
    scratch.write(&at("jobs/0/secret"), "only its user's\n");
    fs::create_dir(record.join("jobs/2")).unwrap();
    // Modes of the record's own, whatever this machine's umask.
    for path in walked(&record, &[]) {
        fs::set_permissions(record.join(path), Permissions::from_mode(0o644)).unwrap();
    }
    fs::set_permissions(record.join("jobs/0/secret"), Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(record.join("model/marked"), Permissions::from_mode(0o7755)).unwrap();
    // Left by copies of the run: one killed, one under way, and two of no copy's name.
    let runs = scratch.0.join("repo/.pumpkin/runs/alice");
    for left in ["a1B2c3", "locked", "a1B2c3d", "a1-2c3"] {
        scratch.write(
            &format!("repo/.pumpkin/runs/alice/.{id}.{left}/sent/x"),
            "left\n",
        );
    }
    let locked = File::open(runs.join(format!(".{id}.locked"))).unwrap();
    locked.lock().unwrap();
    let _ = calls(&scratch);
    let mut lines = Vec::new();
    for step in ["unread", "whole", "unchanged", "changed"] {
        if step == "whole" {
            let secret = Permissions::from_mode(0o600);
            fs::set_permissions(record.join("jobs/0/secret"), secret).unwrap();
        }
        if step == "changed" {
            // The same size, but an older time, as a file written anew in place.
            scratch.write(&at("log.jsonl"), "{\"a\": 2}\n");
            let log = File::options().write(true).open(record.join("log.jsonl"));
            let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
            log.unwrap().set_modified(old).unwrap();
            scratch.write(&at("model/added"), "new\n");
            let added = Permissions::from_mode(0o644);
            fs::set_permissions(record.join("model/added"), added).unwrap();
            fs::remove_file(record.join("jobs/0/out")).unwrap();
        }
        lines.push(format!("{step}:"));
        let command = if ["unchanged", "changed"].contains(&step) {
            "cancel"
        } else {
            "wait"
        };
        lines.extend(copied_record(&scratch, &client, &[command, &id], &copy));
        let copied = scratch.0.join(format!("home/jobs/{id}/copied"));
        lines.push(format!("  noted as copied: {}", copied.exists()));
        if step == "whole" {
            let marked = fs::metadata(copy.join("model/marked")).unwrap().mode();
            lines.push(format!("  model/marked: {:o}", marked & 0o7777));
            let jobs = fs::read_dir(copy.join("jobs")).unwrap();
            let mut jobs: Vec<String> = jobs
                .map(|entry| entry.unwrap().file_name().display().to_string())
                .collect();
            jobs.sort();
            lines.push(format!("  jobs: {}", jobs.join(", ")));
            let marked = Permissions::from_mode(0o755);
            fs::set_permissions(record.join("model/marked"), marked).unwrap();
        }
    }
    let mut kept = record_files(&copy);
    kept.retain(|file| !record_files(&record).contains(file));
    let kept = kept.iter().map(|file| file.splitn(5, ' ').nth(4).unwrap());
    lines.push(format!(
        "only here: {}",
        kept.collect::<Vec<_>>().join(", ")
    ));
    // A cut in the second file leaves only the first complete.
    let big = |seed: u8| -> Vec<u8> { (0..100_000u32).map(|i| seed ^ (i % 251) as u8).collect() };
    fs::write(record.join("model/big-1"), big(1)).unwrap();
    fs::write(record.join("model/big-2"), big(2)).unwrap();
    fs::write(record.join("model/big-3"), big(3)).unwrap();
    for big in ["big-1", "big-2", "big-3"] {
        let path = record.join("model").join(big);
        fs::set_permissions(path, Permissions::from_mode(0o755)).unwrap();
    }
    let cut = scratch.0.join("bin/cut-out");
    let before_copy = format!("echo 150000 > '{}'\n", cut.display());
    scratch.write("bin/before-1", &before_copy);
    // Withhold driver.pid before the retry to observe exactly what the cut placed.
    scratch.write(
        "bin/before-2",
        &format!(
            "mv '{}' '{}'\n",
            record.join("driver.pid").display(),
            record.join(".held-pid").display()
        ),
    );
    lines.push("cut, then refused:".to_owned());
    lines.extend(copied_record(&scratch, &client, &["cancel", &id], &copy));
    fs::rename(record.join(".held-pid"), record.join("driver.pid")).unwrap();
    scratch.write("bin/before-1", &before_copy);
    lines.push("cut:".to_owned());
    lines.extend(copied_record(&scratch, &client, &["cancel", &id], &copy));
    let mut here = record_files(&copy);
    here.retain(|file| !file.ends_with(" jobs/0/out"));
    assert_eq!(here, record_files(&record));
    lines.push("the copy (but `jobs/0/out`) is the record's:".to_owned());
    lines.extend(here.iter().map(|file| {
        let mut words = file.splitn(5, ' ');
        let mode = words.next().unwrap();
        format!("  {} {mode}", words.nth(3).unwrap())
    }));
    let runs = fs::read_dir(runs).unwrap();
    let mut runs: Vec<String> = runs
        .map(|entry| entry.unwrap().file_name().display().to_string())
        .collect();
    runs.sort();
    lines.push(format!("runs: {}", runs.join(", ")));
    check(
        &scratch,
        &lines.join("\n"),
        &[(id.as_str(), "<id>"), (&hex, "<this build>")],
        &expect![[r#"
            unread:
            wait: exit status: 1
              pumpkin: controller `alice` copying run <id>: <scratch>/home/.pumpkin/runs/<id>/jobs/0/secret: Permission denied (os error 13): the copy keeps the files that came whole. Once the controller can read the record, copy it again
              the calls: serve=.pumpkin/<this build>/bin/pumpkin
              written: driver.pid, jobs/0/out, log.jsonl, model/a\nb, model/a b, model/marked, outcome.json
              noted as copied: false
            whole:
            wait: exit status: 0
              the calls: serve=.pumpkin/<this build>/bin/pumpkin
              written: jobs/0/secret, record.json
              noted as copied: true
              model/marked: 755
              jobs: 0, 2
            unchanged:
            cancel: exit status: 0
              pumpkin: stopped job <id> on controller `alice`; copied its record to <scratch>/repo/.pumpkin/runs/alice/<id>: read it with the pumpkin that made it
              the calls: serve=.pumpkin/<this build>/bin/pumpkin
              written: 
              noted as copied: true
            changed:
            cancel: exit status: 0
              pumpkin: stopped job <id> on controller `alice`; copied its record to <scratch>/repo/.pumpkin/runs/alice/<id>: read it with the pumpkin that made it
              the calls: serve=.pumpkin/<this build>/bin/pumpkin
              written: log.jsonl, model/added
              noted as copied: true
            only here: jobs/0/out
            cut, then refused:
            cancel: exit status: 1
              pumpkin: controller `alice` didn't answer: ended within the answer: copying run <id>'s record again now, attempt 2 of 7
              the calls: serve=.pumpkin/<this build>/bin/pumpkin, serve=.pumpkin/<this build>/bin/pumpkin
              written: model/big-1
            cut:
            cancel: exit status: 0
              pumpkin: controller `alice` didn't answer: ended within the answer: copying run <id>'s record again now, attempt 2 of 7
              pumpkin: stopped job <id> on controller `alice`; copied its record to <scratch>/repo/.pumpkin/runs/alice/<id>: read it with the pumpkin that made it
              the calls: serve=.pumpkin/<this build>/bin/pumpkin, serve=.pumpkin/<this build>/bin/pumpkin
              written: model/big-2, model/big-3
            the copy (but `jobs/0/out`) is the record's:
              driver.pid 644
              jobs/0/secret 600
              log.jsonl 644
              model/a\nb 644
              model/a b 644
              model/added 644
              model/big-1 755
              model/big-2 755
              model/big-3 755
              model/marked 755
              outcome.json 644
              record.json 644
            runs: .<id>.a1-2c3, .<id>.a1B2c3d, .<id>.locked, <id>"#]],
    );
}

#[test]
fn copy_with_unreadable_directories() {
    let scratch = Scratch::new();
    let client = set_up_client(&scratch);
    connect(&scratch, &client);
    let hex = served(&scratch);
    let id = id_ago(0, "0d000003");
    ended_job(&scratch, &id, "built\n");
    symlink(
        scratch.0.join(format!("home/.pumpkin/{hex}/bin/pumpkin")),
        scratch.0.join(format!("home/jobs/{id}/pumpkin")),
    )
    .unwrap();
    let record = scratch.0.join(format!("home/.pumpkin/runs/{id}"));
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/alice/{id}"));
    let remote = record.join("private");
    let local = copy.join("local");
    fs::create_dir(&remote).unwrap();
    fs::write(remote.join("output"), "whole").unwrap();
    fs::create_dir_all(&local).unwrap();
    fs::set_permissions(&local, Permissions::from_mode(0o000)).unwrap();
    let mut lines = Vec::new();
    for mode in [0o000, 0o700] {
        fs::set_permissions(&remote, Permissions::from_mode(mode)).unwrap();
        let output = on_client(&scratch, &client, &["cancel", &id])
            .output()
            .unwrap();
        lines.push(output.status.to_string());
        lines.extend(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter(|line| line.contains("Permission denied"))
                .map(str::to_owned),
        );
        lines.push(format!(
            "outcome.json: {}, private/output: {}, record.json: {}",
            copy.join("outcome.json").exists(),
            copy.join("private/output").exists(),
            copy.join("record.json").exists()
        ));
    }
    fs::set_permissions(&local, Permissions::from_mode(0o700)).unwrap();
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 1
            pumpkin: controller `alice` copying run <id>: <scratch>/home/.pumpkin/runs/<id>/private: Permission denied (os error 13): the copy keeps the files that came whole. Once the controller can read the record, copy it again
            outcome.json: true, private/output: false, record.json: false
            exit status: 0
            outcome.json: true, private/output: true, record.json: true"#]],
    );
}

#[test]
fn copy_refuses_a_live_run_or_linked_directory() {
    let scratch = Scratch::new();
    let client = connected_client(&scratch);
    let id = id_ago(0, "0d000002");
    ended_job(&scratch, &id, "built\n");
    let record = scratch.0.join(format!("home/.pumpkin/runs/{id}"));
    let mut request = Vec::new();
    schema::frame::write(
        &mut request,
        &json!({"type":"copy","id":id,"held_bytes":0}),
        &[],
    )
    .unwrap();
    let driver = lock_driver(&record);
    let mut lines = vec![serve_fed(&scratch, &request)];
    drop(driver);
    let copy = scratch.0.join(format!("repo/.pumpkin/runs/alice/{id}"));
    fs::create_dir_all(&copy).unwrap();
    fs::create_dir(scratch.0.join("outside")).unwrap();
    symlink(scratch.0.join("outside"), copy.join("model")).unwrap();
    fs::create_dir(record.join("model")).unwrap();
    fs::write(record.join("model/input"), "private").unwrap();
    let output = on_client(&scratch, &client, &["cancel", &id])
        .output()
        .unwrap();
    lines.push(output.status.to_string());
    lines.extend(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter(|s| s.contains("expected a directory"))
            .map(str::to_owned),
    );
    lines.push(format!(
        "files outside the copy: {}",
        fs::read_dir(scratch.0.join("outside")).unwrap().count()
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[(&id, "<id>")],
        &expect![[r#"
            exit status: 0
              {"type":"serving"}, and a body of 0 bytes
              {"message":"run <id> is still writing its record: wait until it ends and copy it again","type":"refused"}, and a body of 0 bytes
            exit status: 1
            pumpkin: linking to controller `alice` failed: <scratch>/repo/.pumpkin/runs/alice/<id>/model is not a directory: expected a directory inside the copy
            files outside the copy: 0"#]],
    );
}

fn viewed(scratch: &Scratch) -> ([(String, String); 2], File, [&'static str; 2]) {
    let client = set_up_client(scratch);
    connect(scratch, &client);
    let (ended, live) = ("20261110-090000-0000000a", "20261110-110000-0000000c");
    let places = lease_places(Prices::Both);
    let outcome = returned(49.36);
    let dir = write_run(
        scratch,
        ended,
        asked(ended, 30),
        &whole_log(),
        Some(outcome),
        &places,
    );
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(dir.join("log.jsonl"))
        .unwrap();
    // More than a read of the log holds.
    for index in 0..3000 {
        let padding = format!("{index:0>85}");
        std::io::Write::write_all(
            &mut log,
            format!("{{\"padding\":\"{padding}\"}}\n").as_bytes(),
        )
        .unwrap();
    }
    let dir = write_run(
        scratch,
        live,
        asked(live, 30),
        &cut(&whole_log(), 130),
        None,
        &places,
    );
    let driver = lock_driver(&dir);
    fs::create_dir_all(scratch.0.join("home/.pumpkin")).unwrap();
    fs::rename(scratch.0.join("runs"), scratch.0.join("home/.pumpkin/runs")).unwrap();
    (client, driver, [ended, live])
}

// The length of the ended run's log, which `viewed` pads, in the form that `check` takes to mask.
fn log_bytes(scratch: &Scratch, ended: &str) -> String {
    let log = scratch
        .0
        .join(format!("home/.pumpkin/runs/{ended}/log.jsonl"));
    fs::metadata(log).unwrap().len().to_string()
}

fn got(served: &Served, path: &str, fields: &[&str]) -> String {
    let (status, body) = served.ask("GET", path, &[]);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    if let Some(error) = body.get("error") {
        return format!("GET {path}\n{status}\n{error}");
    }
    let fields: Vec<String> = (fields.iter())
        .map(|field| format!("{field}: {}", body[field]))
        .collect();
    format!("GET {path}\n{status}\n{}", fields.join(", "))
}

#[test]
fn views_the_controllers_runs_over_a_reopened_connection() {
    let scratch = Scratch::new();
    let (client, driver, [ended, live]) = viewed(&scratch);
    served(&scratch);
    // The ended run's job is a member's; no job started the live one.
    scratch.write(&format!("home/jobs/{ended}/member"), "alice\n");
    scratch.write("bin/cut-out", "1000");
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let mut steps = vec![served.said.trim_end().to_owned()];
    let window = format!("/api/runs/{ended}/log?at=0");
    steps.push(got(&served, &window, &["next", "end"]));
    assert!(!scratch.0.join("bin/cut-out").exists());
    let (status, listed) = served.ask("GET", "/api/runs", &[]);
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let rows: Vec<String> = (listed["runs"].as_array().unwrap().iter())
        .map(|row| format!("{} {} {}", row["id"], row["state"], row["member"]))
        .collect();
    steps.push(format!("GET /api/runs\n{status}\n{}", rows.join("\n")));
    steps.push(got(
        &served,
        &format!("/api/runs/{ended}"),
        &["state", "end"],
    ));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[
            (ended, "<ended>"),
            (live, "<live>"),
            (&log_bytes(&scratch, ended), "<log bytes>"),
        ],
        &expect![[r#"
            pumpkin: serving controller `alice`'s runs to this machine alone, until stopped, at
                http://127.0.0.1:<port>/?t=<token>
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            next: <log bytes>, end: <log bytes>
            GET /api/runs
            HTTP/1.1 200 OK
            "<live>" "work" null
            "<ended>" "finished" "alice"
            GET /api/runs/<ended>
            HTTP/1.1 200 OK
            state: "finished", end: <log bytes>"#]],
    );
}

#[test]
fn view_answers_502_once_the_reopened_connection_is_cut_too_then_recovers() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    served(&scratch);
    let calls = fs::read_to_string(scratch.0.join("bin/scripts")).unwrap_or_default();
    let cut_out = scratch.0.join("bin/cut-out");
    scratch.write("bin/cut-out", "1000");
    // So that the connection opened anew is cut too.
    let again = format!("echo 1000 > {}\n", cut_out.display());
    scratch.write(&format!("bin/before-{}", calls.lines().count() + 2), &again);
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut steps = vec![got(&served, &window, &["next", "end"])];
    assert!(!cut_out.exists());
    steps.push(got(&served, &window, &["next", "end"]));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[
            (ended, "<ended>"),
            (&log_bytes(&scratch, ended), "<log bytes>"),
        ],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "controller `alice` didn't answer: ended within the answer's body"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            next: <log bytes>, end: <log bytes>"#]],
    );
}

#[test]
fn view_answers_502_for_a_reply_it_cant_read() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    let hex = served(&scratch);
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut hello = Vec::new();
    schema::frame::write(&mut hello, &json!({ "type": "serving" }), &[]).unwrap();
    fs::write(scratch.0.join("hello"), hello).unwrap();
    // Unlinked first, as it is a link to this build. It says its hello, takes the request's
    // length, which is all that the link needs of it, and sends the reply.
    let exe = format!("home/.pumpkin/{hex}/bin/pumpkin");
    fs::remove_file(scratch.0.join(&exe)).unwrap();
    let script = format!(
        "#!/bin/sh\ncat {0}/hello\nhead --bytes=4 > /dev/null\ncat {0}/reply\n",
        scratch.0.display()
    );
    scratch.write_executable(&exe, script.as_bytes());
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let mut past = (u32::try_from(schema::frame::MAX_MESSAGE_BYTES).unwrap() + 1)
        .to_be_bytes()
        .to_vec();
    past.extend(b"{}");
    let mut body_past = Vec::new();
    let answer = json!({
        "type": "viewed", "status": "200 OK", "content_type": "text/plain", "deflated": false,
        "body_bytes": 256 * 1024 * 1024 + 1,
    });
    schema::frame::write(&mut body_past, &answer, &[]).unwrap();
    let mut other = Vec::new();
    schema::frame::write(&mut other, &json!({ "type": "reap" }), &[]).unwrap();
    let mut steps = Vec::new();
    for reply in [past, body_past, other] {
        fs::write(scratch.0.join("reply"), reply).unwrap();
        steps.push(got(&served, &window, &[]));
    }
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(ended, "<ended>")],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "linking to controller `alice` failed: the answer is no frame that `pumpkin controller serve` sends: a frame's message is 65537 bytes: it holds at most 65536: expected its frames alone on the standard output there, which a shell's startup file may write to"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "linking to controller `alice` failed: its answer's body is of 268435457 bytes: expected 256 MiB at most"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "linking to controller `alice` failed: the answer is no frame that `pumpkin controller serve` sends: it is neither an answer to the request nor a refusal: expected its frames alone on the standard output there, which a shell's startup file may write to""#]],
    );
}

#[test]
fn view_asks_ec2_once_no_link_opens() {
    let scratch = Scratch::new();
    let (client, driver, [ended, _]) = viewed(&scratch);
    let hex = served(&scratch);
    scratch.write("bin/cut-out", "1000");
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    // A `serve` that ends before its hello, so that no link opens; then EC2 is asked, which the
    // stand-in `aws` refuses.
    let exe = scratch.0.join(format!("home/.pumpkin/{hex}/bin/pumpkin"));
    let aside = scratch.0.join("aside");
    fs::rename(&exe, &aside).unwrap();
    scratch.write_executable(&format!("home/.pumpkin/{hex}/bin/pumpkin"), b"#!/bin/sh\n");
    let window = format!("/api/runs/{ended}/log?at=0");
    let mut steps = vec![got(&served, &window, &["end"])];
    fs::rename(&aside, &exe).unwrap();
    steps.push(got(&served, &window, &["end"]));
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[
            (ended, "<ended>"),
            (&log_bytes(&scratch, ended), "<log bytes>"),
        ],
        &expect![[r#"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 502 Bad Gateway
            "controller `alice` didn't answer: ended before its hello; asking EC2 whether it runs failed too: `aws --version` failed with exit status: 1 and wrote nothing to standard error: expected it to exit with 0"
            GET /api/runs/<ended>/log?at=0
            HTTP/1.1 200 OK
            end: <log bytes>"#]],
    );
}

#[test]
fn view_reopens_between_requests() {
    let scratch = Scratch::new();
    let (client, driver, [id, _]) = viewed(&scratch);
    served(&scratch);
    scratch.write("bin/link-pid", "");
    let page = Served::of(on_client(&scratch, &client, &["view"]));
    let path = format!("/api/runs/{id}");
    let first = got(&page, &path, &["state"]);
    let pid = fs::read_to_string(scratch.0.join("bin/link-pid")).unwrap();
    let pid = pid.trim().parse::<i32>().unwrap();
    // A previously answered connection ends between requests, as when serve's idle time ends.
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    poll(|| (!super::alive(i64::from(pid))).then_some(()));
    let second = got(&page, &path, &["state"]);
    assert_eq!(first, second);
    let connections = fs::read_to_string(scratch.0.join("bin/scripts"))
        .unwrap()
        .lines()
        .count();
    drop(driver);
    check(
        &scratch,
        &format!("{second}\nSSH connections: {connections}"),
        &[(id, "<id>")],
        &expect![[r#"
            GET /api/runs/<id>
            HTTP/1.1 200 OK
            state: "finished"
            SSH connections: 2"#]],
    );
}

// Each is refused before EC2 is asked, which the stand-in `aws` doesn't answer.
#[test]
fn view_refuses_a_start_but_from_its_page() {
    let scratch = Scratch::new();
    let (client, driver, _) = viewed(&scratch);
    let served = Served::of(on_client(&scratch, &client, &["view"]));
    let page = format!("http://127.0.0.1:{}", served.port);
    let start = "/api/controller/start";
    let mut steps = Vec::new();
    for (method, headers) in [
        ("GET", &[][..]),
        ("POST", &[("Origin", "http://pumpkin.example")]),
        ("POST", &[("Origin", &page), ("X-Pumpkin-Token", "")]),
    ] {
        let (status, body) = served.ask(method, start, headers);
        steps.push(format!("{method} {start} {headers:?}\n{status}\n{body}"));
    }
    drop(driver);
    check(
        &scratch,
        &steps.join("\n"),
        &[(&page, "<page>")],
        &expect![[r#"
            GET /api/controller/start []
            HTTP/1.1 405 Method Not Allowed
            {"error":"the controller's start is sent by `POST`, as the page sends it: start it from the page that `pumpkin view` serves, or run a command that reaches it, which starts it: pumpkin ls --controller alice"}
            POST /api/controller/start [("Origin", "http://pumpkin.example")]
            HTTP/1.1 403 Forbidden
            {"error":"the controller's start comes from `http://pumpkin.example`, a page of another site: start it from the page that `pumpkin view` serves, or run a command that reaches it, which starts it: pumpkin ls --controller alice"}
            POST /api/controller/start [("Origin", "<page>"), ("X-Pumpkin-Token", "")]
            HTTP/1.1 401 Unauthorized
            {"error":"the request carries no token in `X-Pumpkin-Token`: give the one in the address that `pumpkin view` printed as it started"}"#]],
    );
}
