//! `controller`, the controller's own side of the controller target, which runs as the
//! controller's user would, with a home and a runtime directory of the scratch directory's, on
//! a stand-in `aws` for the controller's settings ([`AWS`]), a stand-in `curl` for the instance
//! metadata service, a stand-in `df`, and the stand-in `docker` ([`StandIn`]). And the client's
//! side: what it refuses before it reaches a controller, and what it sends one, and where.

use std::ffi::OsStr;
use std::fs::{self, File, Permissions};
use std::io::{BufRead as _, BufReader, Read};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use expect_test::{Expect, expect};
use schema::ir::Sha256Digest;
use serde_json::json;
use stdx::Utc;
use stdx::process::Environment;
use test_support::BUSYBOX;

use super::{
    BUILDS, EC2, IMAGE, INFO, LOCAL, MODAL, Scratch, StandIn, cpu_list, docker_table, ended,
    install, invocation, report, workflow,
};

/// The controller's settings, as the module renders its SSM parameter: the engine, the provider
/// `aws` of kind `ec2`, and the `images` of the provider `gpus` of kind `modal`.
fn parameter() -> String {
    let images = MODAL
        .lines()
        .find(|line| line.starts_with("images"))
        .unwrap();
    format!(
        "{}\n{EC2}\n[providers.gpus.modal]\n{images}\n",
        docker_table("default")
    )
}

/// The Modal provider that the client sends, as its `[controller.modal]` gives it.
const SENT_MODAL: &str = r#"[modal]
profile = "alice"
classes = { a100 = { gpu = "A100-80GB", gpu_model = "NVIDIA A100-SXM4-80GB", cpu_millis = 8000, memory_bytes = 68719476736, startup_seconds = 1800 } }
"#;

/// The stand-in `aws`, which answers `--version`, and `ssm get-parameter` with what
/// `parameter.json` beside it holds ([`write_parameter`]), fails any other call, and notes each
/// call in `calls` beside it, its command and then its arguments, a line each, and the call
/// ended by an empty line.
const AWS: &str = r#"#!/bin/sh
{ echo aws; printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
case "$*" in
    --version) echo aws-cli/2.38.1 Python/3.14.6 Linux/7.0.0 exe/x86_64.stand-in ;;
    *" ssm get-parameter "*) cat "${0%/*}/parameter.json" ;;
    *) echo "the stand-in aws answers only \`--version\` and \`ssm get-parameter\`" >&2; exit 1 ;;
esac
"#;

/// Writes the controller's settings, `text`, as the stand-in `aws` gives them ([`AWS`]).
fn write_parameter(scratch: &Scratch, text: &str) {
    let parameter = json!({ "Parameter": { "Name": "/pumpkin-alice/settings", "Type": "String", "Value": text } });
    scratch.write("bin/parameter.json", &parameter);
}

/// The stand-in `curl`, which answers as the instance metadata service of an instance whose
/// private IPv4 address is 172.31.5.6 and whose public one is 198.51.100.9, and notes each call
/// in `calls` beside it, as [`AWS`] does.
const CURL: &str = r#"#!/bin/sh
{ echo curl; printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
for url; do :; done
case $url in
    */api/token) echo stand-in-token ;;
    */meta-data/local-ipv4) echo 172.31.5.6 ;;
    */meta-data/public-ipv4) echo 198.51.100.9 ;;
    *) echo "curl: (22) The requested URL returned error: 404" >&2; exit 22 ;;
esac
"#;

/// The stand-in `df`, which says that the disk is of the size and has as much available, in
/// bytes, as `disk` beside it says, then, if `disk.next` is there, takes it as `disk`; and notes
/// each call in `calls`, as [`CURL`] does.
const DF: &str = r#"#!/bin/sh
{ echo df; printf '%s\n' "$@"; echo; } >> "${0%/*}/calls"
echo '1B-blocks Avail'
cat "${0%/*}/disk"
[ ! -e "${0%/*}/disk.next" ] || mv "${0%/*}/disk.next" "${0%/*}/disk"
"#;

/// The id of a run `days` ago, as `pumpkin run` mints one, with `random` as its random digits.
fn id_ago(days: u64, random: &str) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let then = now.checked_sub(Duration::from_hours(days * 24)).unwrap();
    let Utc {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = Utc::from_unix(then.as_secs());
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{random}")
}

/// An image's ID, of 64 of `digit`.
fn image(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

/// Sets `scratch`, which holds the stand-in `docker` ([`StandIn`]), up as the controller: the
/// other stand-ins in `bin`, the controller user's home in `home`, with the `pumpkin` of
/// [`PINNED`] built in its one workspace, and its runtime directory in `run`, with the module's
/// parameter, and a disk that is half used.
fn set_up(scratch: &Scratch) {
    scratch.write_executable("bin/aws", AWS.as_bytes());
    scratch.write_executable("bin/curl", CURL.as_bytes());
    scratch.write_executable("bin/df", DF.as_bytes());
    scratch.write("bin/disk", "1000 500\n");
    write_parameter(scratch, &parameter());
    install(scratch, &format!("home/.pumpkin/{PINNED}/bin"));
    fs::create_dir_all(scratch.0.join("run")).unwrap();
    let empty = format!("home/store/manifests/{EMPTY_TREE}");
    scratch.write(&empty, "pumpkin-tree/1\n");
}

/// The SHA-256 of the manifest of a tree of no files, `pumpkin-tree/1` and a newline, which
/// [`set_up`] keeps in the controller's store.
const EMPTY_TREE: &str = "dff3cfced0b99bbac128797b268e1756972cb1285a305e2dc1b9d96e97e6eb5e";

/// The `pumpkin` that [`set_up`] built on the controller.
fn controllers(scratch: &Scratch) -> PathBuf {
    scratch
        .0
        .join(format!("home/.pumpkin/{PINNED}/bin/pumpkin"))
}

/// `pumpkin` with `args`, as the controller's user runs it in a unit of its own.
fn on_controller(scratch: &Scratch, args: &[&str]) -> Command {
    let mut command = scratch.program(controllers(scratch), args);
    command
        .env("HOME", scratch.0.join("home"))
        .env("XDG_RUNTIME_DIR", scratch.0.join("run"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", scratch.0.join("bin").display()),
        );
    command
}

/// Writes what the client sends with job `id`: what it sends of its configuration, `config`, the
/// key of upstream `gateway`, and, if `modal`, its Modal configuration file.
fn send(scratch: &Scratch, id: &str, config: &str, modal: bool) {
    let sent = format!("run/pumpkin/{id}");
    scratch.write(&format!("{sent}/config.toml"), config);
    scratch.write(&format!("{sent}/gateway.key"), "sk-stand-in\n");
    if modal {
        scratch.write(
            &format!("{sent}/modal.toml"),
            "[pumpkin]\ntoken_id = \"ak-x\"\n",
        );
    }
    fs::set_permissions(scratch.0.join(sent), Permissions::from_mode(0o700)).unwrap();
}

/// The upstreams that the client sends, each `key_file` a file it sends beside them.
const UPSTREAMS: &str = r#"[upstreams.gateway]
url = "https://gateway.example/v1"
key_file = "gateway.key"
models = [{ name = "model-a", upstream_name = "provider/model-a" }]
"#;

/// What the stand-ins noted, `calls`, `aws`'s, `curl`'s and `df`'s, and `docker.pruned`, each
/// taken.
fn noted(scratch: &Scratch) -> Vec<String> {
    let taken = |file: &str| {
        let path = scratch.0.join(file);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(path);
        text
    };
    let calls = taken("bin/calls");
    let calls =
        (calls.split_terminator("\n\n")).map(|call| call.lines().collect::<Vec<_>>().join(" "));
    let mut lines: Vec<String> = calls.collect();
    lines.extend(taken("docker.pruned").lines().map(str::to_owned));
    lines
}

/// Checks `text`, with the scratch directory as `<scratch>`, against `expect`.
fn check(scratch: &Scratch, text: &str, shown: &[(&str, &str)], expect: &Expect) {
    let mut text = text.replace(scratch.0.to_str().unwrap(), "<scratch>");
    text = text.replace(EMPTY_TREE, "<empty tree>");
    for (value, name) in shown {
        text = text.replace(value, name);
    }
    // How long this machine has been up, which the controller's health says of the stand-in.
    let lines = text.lines().map(|line| match line.split_once("`: up ") {
        Some((before, after)) => {
            let rest = after.split_once(',').map_or("", |(_, rest)| rest);
            format!("{before}`: up <t>,{rest}")
        }
        None => line.to_owned(),
    });
    expect.assert_eq(&lines.collect::<Vec<String>>().join("\n"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the configuration's snapshot"
)]
fn job() {
    let scratch = Scratch::new("controller-job");
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let modal = MODAL.replace("dials_from = \"203.0.113.7\"\n", "");
    let parameter = format!("{}\n{EC2}\n{modal}", docker_table("default"));
    write_parameter(&scratch, &parameter);
    let id = id_ago(0, "0b000001");
    let (recent, old) = (id_ago(3, "0b000002"), id_ago(30, "0b000003"));
    let (built, failed) = (id_ago(1, "0b000007"), id_ago(2, "0b000008"));
    send(&scratch, &id, &format!("{UPSTREAMS}\n{SENT_MODAL}"), true);
    let ir = |digits: &str| {
        let roles = digits.chars().map(|digit| json!({ "image": image(digit) }));
        json!({ "roles": roles.collect::<Vec<_>>() })
    };
    // A run of 3 days ago, naming the stand-in's image and another in its record's IR, and one of
    // 30, naming one; a build of 1 day ago naming one in its `out`, and a failed one of 2, whose
    // `out` is empty.
    for (run, digits) in [(&recent, "0a"), (&old, "b")] {
        scratch.write(&format!("home/jobs/{run}/pumpkin.toml"), "");
        let record = format!("home/.pumpkin/runs/{run}");
        scratch.write(&format!("{record}/ir.json"), &ir(digits));
    }
    // Jobs of 30 days ago and of 1 that ended before their runs made a record.
    let (stale, fresh) = (id_ago(30, "0b00000a"), id_ago(1, "0b00000b"));
    for unstarted in [&stale, &fresh] {
        scratch.write(&format!("home/jobs/{unstarted}/log"), "");
    }
    scratch.write(&format!("home/builds/{built}/out"), &ir("c"));
    scratch.write(&format!("home/builds/{failed}/out"), "");
    // Image c bears the tag that the other job below holds.
    let held = format!("pumpkin/held:{}", "c".repeat(64));
    let sized = ['0', 'a', 'b', 'c', 'd'].map(|digit| match digit {
        'c' => format!("{} 100 {held}\n", image(digit)),
        _ => format!("{} 100\n", image(digit)),
    });
    scratch.write("images", &sized.concat());
    // 850 of its 1000 bytes used.
    scratch.write("bin/disk", "1000 150\n");
    for tree in [&recent, &old, &".pumpkin".to_owned()] {
        scratch.write(&format!("home/trees/tasks/{tree}/pumpkin"), "");
    }
    let mut job = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    // As its unit's `StandardOutput=` does.
    fs::create_dir_all(scratch.0.join(format!("home/builds/{id}"))).unwrap();
    let out = scratch.0.join(format!("home/builds/{id}/out"));
    job.stdout(File::create(&out).unwrap());
    let invoked = invocation(&job);
    let mut lines = vec![report(invoked, &job.output().unwrap(), &scratch.0, false)];
    let ir = fs::read_to_string(&out).unwrap();
    lines.push(format!("out names the image: {}", ir.contains(&image('0'))));
    let current = fs::read_link(scratch.0.join("home/current-pumpkin")).unwrap();
    lines.push(format!(
        "current-pumpkin is this pumpkin: {}",
        current == controllers(&scratch)
    ));
    let config = fs::read_to_string(scratch.0.join(format!("home/builds/{id}/pumpkin.toml")));
    lines.push(format!("pumpkin.toml:\n{}", config.unwrap()));
    lines.extend(noted(&scratch));
    let names = |dir: &str| {
        let entries = fs::read_dir(scratch.0.join(dir)).unwrap();
        let mut names: Vec<String> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names.join(", ")
    };
    lines.push(format!("trees: {}", names("home/trees/tasks")));
    lines.push(format!("jobs: {}", names("home/jobs")));
    // Another job's directory in tmpfs, as while it runs, its run started, with the volume full.
    send(&scratch, &recent, UPSTREAMS, false);
    let log = format!("home/.pumpkin/runs/{recent}/log.jsonl");
    scratch.write(&log, "");
    scratch.write("bin/disk", "1000 0\n");
    for started in [true, false] {
        if !started {
            fs::remove_file(scratch.0.join(&log)).unwrap();
            scratch.write(&format!("run/pumpkin/{recent}/held"), &format!("{held}\n"));
        }
        send(&scratch, &id, UPSTREAMS, false);
        let mut again = on_controller(
            &scratch,
            &[
                "controller",
                "job",
                &id,
                "--region",
                "us-west-1",
                "--parameter",
                "/pumpkin-alice/settings",
                "--tree",
                EMPTY_TREE,
                "--",
                "build",
                "stand-in/task.star",
            ],
        );
        again.stdout(Stdio::null());
        let invoked = invocation(&again);
        lines.push(format!("another job's run started: {started}"));
        lines.push(report(invoked, &again.output().unwrap(), &scratch.0, false));
        lines.extend(
            noted(&scratch)
                .into_iter()
                .filter(|line| line.starts_with("docker")),
        );
    }
    // Full as the prune begins, and no longer once it has the lock.
    scratch.write("bin/disk", "1000 0\n");
    scratch.write("bin/disk.next", "1000 500\n");
    send(&scratch, &id, UPSTREAMS, false);
    let mut freed = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    freed.stdout(Stdio::null());
    let invoked = invocation(&freed);
    lines.push("freed while the prune waited for the lock:".to_owned());
    lines.push(report(invoked, &freed.output().unwrap(), &scratch.0, false));
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker") || line.starts_with("df")),
    );
    let config = fs::read_to_string(scratch.0.join(format!("home/builds/{id}/pumpkin.toml")));
    let config: toml::Table = toml::from_str(&config.unwrap()).unwrap();
    let providers: Vec<&String> = config["providers"].as_table().unwrap().keys().collect();
    lines.push(format!("the providers: {providers:?}"));
    let shown = [
        (id.as_str(), "<id>"),
        (recent.as_str(), "<recent>"),
        (old.as_str(), "<old>"),
        (built.as_str(), "<built>"),
        (failed.as_str(), "<failed>"),
        (stale.as_str(), "<stale>"),
        (fresh.as_str(), "<fresh>"),
    ];
    check(
        &scratch,
        &lines.join("\n"),
        &shown,
        &expect![[r#"
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            out names the image: true
            current-pumpkin is this pumpkin: true
            pumpkin.toml:
            [docker]
            context = "default"

            [gateway]
            private = "172.31.5.6:0"

            [providers.aws.ec2]
            region = "us-west-1"

            [providers.aws.ec2.classes.agent]
            memory_mib = 262144
            template = "lt-0aaaaaaaaaaaaaaaa"
            vcpus = 32
            version = 7

            [providers.aws.ec2.classes.judge]
            memory_mib = 16384
            template = "lt-0bbbbbbbbbbbbbbbb"
            vcpus = 8
            version = 2

            [providers.aws.ec2.repositories]
            roles = "pumpkin/roles"
            sandboxd = "pumpkin/sandboxd"

            [providers.aws.ec2.timer]
            group = "pumpkin"
            role_arn = "arn:aws:iam::123456789012:role/pumpkin-timer"

            [providers.gpus.modal]
            config = "<scratch>/run/pumpkin/<id>/modal.toml"
            dials_from = "198.51.100.9"
            profile = "alice"

            [providers.gpus.modal.classes.a100]
            cpu_millis = 8000
            gpu = "A100-80GB"
            gpu_model = "NVIDIA A100-SXM4-80GB"
            memory_bytes = 68719476736
            startup_seconds = 1800

            [providers.gpus.modal.images]
            region = "us-west-1"
            repository = "pumpkin/roles"
            role_arn = "arn:aws:iam::123456789012:role/pumpkin-modal-pull"

            [upstreams.gateway]
            key_file = "<scratch>/run/pumpkin/<id>/gateway.key"
            url = "https://gateway.example/v1"

            [[upstreams.gateway.models]]
            name = "model-a"
            upstream_name = "provider/model-a"

            aws --version
            aws --no-cli-pager --output json --region us-west-1 ssm get-parameter --name /pumpkin-alice/settings
            curl --disable --silent --show-error --fail --max-time 10 --request PUT --header X-aws-ec2-metadata-token-ttl-seconds: 60 http://169.254.169.254/latest/api/token
            curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/local-ipv4
            curl --disable --silent --show-error --fail --max-time 10 --header X-aws-ec2-metadata-token: stand-in-token http://169.254.169.254/latest/meta-data/public-ipv4
            df --block-size=1 --output=size,avail -- /
            df --block-size=1 --output=size,avail -- /
            docker image rm --force sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
            docker image rm --force sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            docker image rm --force sha256:0000000000000000000000000000000000000000000000000000000000000000
            docker builder prune --force --min-free-space 400
            trees: .pumpkin, <recent>
            jobs: <old>, <recent>, <fresh>
            another job's run started: true
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            docker image rm --force sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
            docker image rm --force sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            docker image rm --force sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
            docker builder prune --force --min-free-space 400
            another job's run started: false
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            docker image rm --force sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
            docker image rm --force sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
            docker image rm --force sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
            docker builder prune --force --min-free-space 400
            freed while the prune waited for the lock:
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(0)
            df --block-size=1 --output=size,avail -- /
            df --block-size=1 --output=size,avail -- /
            the providers: ["aws"]"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn job_refusals() {
    let scratch = Scratch::new("controller-job-refusals");
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let id = id_ago(0, "0b000004");
    let job = |scratch: &Scratch, tree: &str| {
        on_controller(
            scratch,
            &[
                "controller",
                "job",
                &id,
                "--region",
                "us-west-1",
                "--parameter",
                "/pumpkin-alice/settings",
                "--tree",
                tree,
                "--",
                "build",
                "stand-in/task.star",
            ],
        )
    };
    let mut lines = Vec::new();
    let cases = [
        (
            "a parameter with [gateway]",
            format!("{}\n[gateway]\nprivate = \"10.0.0.1:0\"\n", parameter()),
            UPSTREAMS.to_owned(),
        ),
        (
            "a parameter with no Modal provider",
            format!("{}\n{EC2}", docker_table("default")),
            format!("{UPSTREAMS}\n{SENT_MODAL}"),
        ),
        (
            "a sent [docker]",
            parameter(),
            format!("{UPSTREAMS}\n[docker]\ncontext = \"elsewhere\"\n"),
        ),
        (
            "a key_file that names a path",
            parameter(),
            UPSTREAMS.replace("\"gateway.key\"", "\"/etc/shadow\""),
        ),
    ];
    for (case, parameter, sent) in cases {
        write_parameter(&scratch, &parameter);
        send(&scratch, &id, &sent, false);
        let mut job = job(&scratch, EMPTY_TREE);
        lines.push(format!("{case}:"));
        let invoked = invocation(&job);
        lines.push(report(invoked, &job.output().unwrap(), &scratch.0, false));
    }
    let mut unset = job(&scratch, EMPTY_TREE);
    unset.env_remove("XDG_RUNTIME_DIR");
    let invoked = invocation(&unset);
    lines.push(report(invoked, &unset.output().unwrap(), &scratch.0, false));
    // A tree with a file below its `.pumpkin`, the controller's workspace; one made in the
    // scratch directory, which is no job's tree.
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
        let mut refused = job(&scratch, &tree);
        let invoked = invocation(&refused);
        lines.push(report(
            invoked,
            &refused.output().unwrap(),
            &scratch.0,
            false,
        ));
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
            a parameter with [gateway]:
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: the controller's settings, SSM parameter `/pumpkin-alice/settings`, hold `[gateway]`, which each job sets: remove it from them
            a parameter with no Modal provider:
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: the controller's settings, SSM parameter `/pumpkin-alice/settings`, name no Modal provider, whose `images` the client's `[controller.modal]` needs: apply `deploy/aws`, whose settings name one
            a sent [docker]:
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: what the run sent, config.toml, is refused: TOML parse error at line 6, column 2
              |
            6 | [docker]
              |  ^^^^^^
            unknown field `docker`, expected `upstreams` or `modal`
            a key_file that names a path:
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: what the run sent gives upstream `gateway` no `key_file` that names a file it sent: send each key as a file of its own
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin env --unset=XDG_RUNTIME_DIR pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <empty tree> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: XDG_RUNTIME_DIR is unset or empty: run the job in a unit of the user's systemd, as the controller target starts it
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <tree of .pumpkin/runs/planted/record> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: <scratch>/home/store/manifests/<tree of .pumpkin/runs/planted/record>: the manifest's line `f <blob> 8 .pumpkin/runs/planted/record` is refused: `.pumpkin/runs/planted/record` can't be sent to the controller, where a job's `.pumpkin` is the controller's own workspace: leave it out of the task
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/run pumpkin controller job <id> --region us-west-1 --parameter /pumpkin-alice/settings --tree <tree of stand-in/notes> -- build stand-in/task.star
            exit status: Some(1)
            pumpkin: the job's working directory, <scratch>, isn't its tree, <scratch>/home/trees/<repository>/<id>, which its start makes: run the job there, as its unit does
            made the file outside a job's tree: false"#]],
    );
}

/// `controller job` `id` of `build built/task.star`, started as the controller's user runs it,
/// but on one CPU of those the tests may run on, so with one build slot.
fn on_one_cpu(scratch: &Scratch, id: &str) -> Child {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let allowed = status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"));
    let cpu = cpu_list(allowed.unwrap()).into_iter().next().unwrap();
    let job = on_controller(
        scratch,
        &[
            "controller",
            "job",
            id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "built/task.star",
        ],
    );
    let mut on_one = stdx::process::command("taskset", Environment::Inherited);
    on_one.args(["--cpu-list", &cpu.to_string()]);
    on_one.arg(job.get_program()).args(job.get_args());
    for (name, value) in job.get_envs() {
        on_one.env(name, value.unwrap());
    }
    on_one.current_dir(job.get_current_dir().unwrap());
    on_one.stdout(Stdio::piped()).stderr(Stdio::piped());
    on_one.spawn().unwrap()
}

/// How `child`, a job that [`on_one_cpu`] started, ended: its exit status; what it said, `said`
/// and then the rest from `stderr`; and whether it printed an IR that names the image that the
/// stand-in builds.
fn job_ended(child: Child, stderr: impl Read, said: String) -> String {
    let output = ended(child, stderr, said);
    let mut lines = vec![format!("exit status: {:?}", output.status.code())];
    lines.extend(
        String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .map(str::to_owned),
    );
    let printed = String::from_utf8(output.stdout).unwrap();
    let built = format!("sha256:{}", "2".repeat(64));
    lines.push(format!(
        "printed the IR of the image built: {}",
        printed.contains(&built)
    ));
    lines.join("\n")
}

/// How `child`, a job that [`on_one_cpu`] started, ended, as [`job_ended`] says.
fn job_ended_whole(mut child: Child) -> String {
    let stderr = child.stderr.take().unwrap();
    job_ended(child, stderr, String::new())
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the jobs' steps and the snapshot"
)]
fn job_admission() {
    let scratch = Scratch::new("controller-job-admission");
    let stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    scratch.write_task("built", &workflow(&image('0'), &["true"]));
    let recipe = format!("FROM {BUSYBOX}\n{BUILDS}\n");
    scratch.write("built/Dockerfile", &recipe);
    let [first, second, third] =
        ["0ad00001", "0ad00002", "0ad00003"].map(|random| id_ago(0, random));
    for id in [&first, &second, &third] {
        send(&scratch, id, UPSTREAMS, false);
    }
    let mut lines = Vec::new();
    let marks = |id: &str| {
        let dir = scratch.0.join(format!("run/pumpkin/{id}"));
        let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names.join(", ")
    };
    stand_in.hold("build");
    let building = on_one_cpu(&scratch, &first);
    stand_in.held("build");
    let held = scratch.0.join(format!("run/pumpkin/{first}/held"));
    let held = fs::read_to_string(held).unwrap();
    lines.push(format!("the first holds: {}", held.trim()));
    lines.push(format!(
        "the first's directory while it builds: {}",
        marks(&first)
    ));
    let tag = held.lines().next().unwrap().to_owned();
    // Image e bears the tag that the first holds, f another, and the stand-in's own none.
    scratch.write(
        "images",
        &format!(
            "{} 100 {tag}\n{} 100 pumpkin/other:{}\n",
            image('e'),
            image('f'),
            "f".repeat(64)
        ),
    );
    scratch.write("bin/disk", "1000 150\n");
    let mut waiting = on_one_cpu(&scratch, &second);
    let mut stderr = BufReader::new(waiting.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting for a build slot") {
        assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
    }
    lines.push(format!(
        "the second's directory while it waits: {}",
        marks(&second)
    ));
    lines.push("the second's prune, while the first builds:".to_owned());
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker")),
    );
    scratch.write("bin/disk", "1000 500\n");
    stand_in.release("build");
    lines.push(format!("the first:\n{}", job_ended_whole(building)));
    let held = scratch.0.join(format!("run/pumpkin/{first}/held"));
    let held = fs::read_to_string(held).unwrap();
    lines.push(format!(
        "the first holds once built: {}",
        held.trim().replace('\n', " ")
    ));
    let waited = job_ended(waiting, stderr, said);
    lines.push(format!("the second:\n{waited}"));
    lines.push(format!(
        "the second's directory once it ended: {}",
        marks(&second)
    ));
    for id in [&first, &second] {
        fs::remove_dir_all(scratch.0.join(format!("run/pumpkin/{id}"))).unwrap();
    }
    fs::remove_file(scratch.0.join("tagged")).unwrap();
    scratch.write("bin/disk", "1000 100\n");
    let refused = on_one_cpu(&scratch, &third);
    lines.push(format!("the third:\n{}", job_ended_whole(refused)));
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker")),
    );
    let mut text = lines.join("\n");
    // How long the second waited.
    let at = text.find("after waiting ").unwrap() + "after waiting ".len();
    let seconds = text[at..].split(' ').next().unwrap().to_owned();
    text.replace_range(at..at + seconds.len(), "<n>");
    check(
        &scratch,
        &text,
        &[
            (first.as_str(), "<first>"),
            (second.as_str(), "<second>"),
            (third.as_str(), "<third>"),
            (tag.strip_prefix("pumpkin/built:").unwrap(), "<hex>"),
        ],
        &expect![[r#"
            the first holds: pumpkin/built:<hex>
            the first's directory while it builds: building, config.toml, gateway.key, held
            the second's directory while it waits: config.toml, gateway.key, held, waiting
            the second's prune, while the first builds:
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400
            the first:
            exit status: Some(0)
            pumpkin: building an image in pumpkin/built
            pumpkin: built an image in pumpkin/built
            printed the IR of the image built: true
            the first holds once built: pumpkin/built:<hex> sha256:2222222222222222222222222222222222222222222222222222222222222222
            the second:
            exit status: Some(0)
            pumpkin: the volume is more than 80% used: the prune removed 1 image, least recently used first
            pumpkin: waiting for a build slot: all 1 held, by jobs <first>
            pumpkin: took a build slot after waiting <n> s
            printed the IR of the image built: true
            the second's directory once it ended: config.toml, gateway.key, held
            the third:
            exit status: Some(1)
            pumpkin: the volume is more than 80% used: the prune removed 3 images, least recently used first
            pumpkin: the volume is more than 80% used: the prune removed 1 image, least recently used first
            built/task.star:1:9: building the image failed: the controller's volume is 90% used, past the 85% at which a build is refused, though the prune has removed what no job holds: free space there, or let the jobs that run end, then run the task again
                1 | IMAGE = image("Dockerfile")
                  |         ^^^^^^^^^^^^^^^^^^^
            pumpkin: built/task.star: the task has 1 problem
            printed the IR of the image built: false
            docker image rm --force sha256:0000000000000000000000000000000000000000000000000000000000000000
            docker image rm --force sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the jobs' steps and the snapshot"
)]
fn job_publish_wait() {
    let scratch = Scratch::new("controller-job-publish-wait");
    // Rootful, as the controller's engine is, of which the job says nothing.
    let rootful = INFO.replace(r#","name=rootless""#, "");
    let _stand_in = StandIn::with(&scratch, &rootful, IMAGE);
    set_up(&scratch);
    write_parameter(
        &scratch,
        &format!("{}\n[providers.local.docker]\n", docker_table("default")),
    );
    scratch.write_sandboxd(&format!("home/.pumpkin/{PINNED}/bin/sandboxd"));
    scratch.write_task("built", &workflow(&image('0'), &["true"]));
    scratch.write("built/Dockerfile", &format!("FROM {BUSYBOX}\n{BUILDS}\n"));
    let [run, other] = ["0ad00011", "0ad00012"].map(|random| id_ago(0, random));
    for id in [&run, &other] {
        send(&scratch, id, UPSTREAMS, false);
    }
    // Each of the engine's 4 publish slots held, as by other runs that publish.
    let slots = scratch.0.join("home/.pumpkin/docker/publish-slots");
    fs::create_dir_all(&slots).unwrap();
    let held: Vec<File> = (0..4)
        .map(|index| {
            let slot = File::create(slots.join(index.to_string())).unwrap();
            slot.lock().unwrap();
            slot
        })
        .collect();
    let mut job = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &run,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "run",
            "built/task.star",
        ],
    );
    let mut publishing = (job.stdout(Stdio::piped()).stderr(Stdio::piped()))
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(publishing.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting for a publish slot") {
        assert!(stderr.read_line(&mut said).unwrap() > 0, "{said}");
    }
    let mut lines = vec![format!("the run's job, as it waits:\n{}", said.trim_end())];
    let named = fs::read_to_string(scratch.0.join(format!("run/pumpkin/{run}/held"))).unwrap();
    lines.push(format!("it holds: {}", named.trim().replace('\n', " ")));
    let hex = named
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("pumpkin/built:");
    let hex = hex.to_owned();
    // The image the run publishes, which another build's tag has moved off, and one that
    // another task's tag names, which no job holds, on a volume past 80% used.
    scratch.write(
        "images",
        &format!(
            "{} 100\n{} 100 pumpkin/other:{}\n",
            image('2'),
            image('f'),
            "f".repeat(64)
        ),
    );
    scratch.write("bin/disk", "1000 150\n");
    noted(&scratch);
    let mut pruning = on_controller(
        &scratch,
        &[
            "controller",
            "job",
            &other,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            EMPTY_TREE,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    let pruned = pruning.stdout(Stdio::null()).output().unwrap();
    lines.push(format!(
        "the other job's prune:\n{}",
        String::from_utf8(pruned.stderr).unwrap().trim_end()
    ));
    lines.extend(
        noted(&scratch)
            .into_iter()
            .filter(|line| line.starts_with("docker")),
    );
    scratch.write("bin/disk", "1000 500\n");
    drop(held);
    let output = ended(publishing, stderr, String::new());
    let record = scratch.0.join(format!("home/.pumpkin/runs/{run}"));
    let ir = fs::read_to_string(record.join("ir.json")).unwrap();
    lines.extend([
        format!("the run's job: exit status {:?}", output.status.code()),
        format!("the run started: {}", record.join("log.jsonl").exists()),
        format!("its IR names the image: {}", ir.contains(&image('2'))),
    ]);
    check(
        &scratch,
        &lines.join("\n"),
        &[
            (run.as_str(), "<run>"),
            (other.as_str(), "<other>"),
            (&hex, "<hex>"),
        ],
        &expect![[r#"
            the run's job, as it waits:
            pumpkin: building an image in pumpkin/built
            pumpkin: built an image in pumpkin/built
            pumpkin: waiting for a publish slot: all 4 held, by 0, 1, 2, 3
            it holds: pumpkin/built:<hex> sha256:2222222222222222222222222222222222222222222222222222222222222222
            the other job's prune:
            pumpkin: the volume is more than 80% used: the prune removed 2 images, least recently used first
            docker image rm --force sha256:0000000000000000000000000000000000000000000000000000000000000000
            docker image rm --force sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
            docker builder prune --force --min-free-space 400
            the run's job: exit status Some(0)
            the run started: true
            its IR names the image: true"#]],
    );
}

// Root's runtime directory, as the boot's unit has it; no prune, as a job that a client started as
// the controller came up may hold an image that no IR names yet.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the records and the snapshot"
)]
fn boot() {
    let scratch = Scratch::new("controller-boot");
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let home = scratch.0.join("home");
    let (clean, local, lost) = (
        id_ago(2, "0b000007"),
        id_ago(1, "0b000005"),
        id_ago(1, "0b000008"),
    );
    let runs = [
        (&clean, "local", docker_table("default") + LOCAL, true),
        (&local, "local", docker_table("default") + LOCAL, false),
        (
            &lost,
            "aws",
            format!("{}\n{EC2}", docker_table("default")),
            false,
        ),
    ];
    for (run, provider, config, ended) in runs {
        let path = home.join(format!("jobs/{run}/pumpkin.toml"));
        scratch.write(&format!("home/jobs/{run}/pumpkin.toml"), &config);
        let record = format!("home/.pumpkin/runs/{run}");
        let args = ["--config".to_owned(), path.display().to_string()];
        scratch.write(
            &format!("{record}/record.json"),
            &json!({ "record_version": "pumpkin.record/v0", "args": args }),
        );
        scratch.write(&format!("{record}/driver.pid"), "4242\n");
        let ir = json!({ "roles": [{ "name": "agent", "image": image('a') }] });
        scratch.write(&format!("{record}/ir.json"), &ir);
        let report = match provider {
            "local" => json!({ "context": "default" }),
            _ => json!({ "region": "us-west-1" }),
        };
        scratch.write(
            &format!("{record}/provider.json"),
            &json!({ "providers": { provider: { "report": report } } }),
        );
        let mut sandbox =
            json!({ "provider": provider, "id": format!("sandbox-{run}"), "report": {} });
        if ended {
            sandbox["delete"] = json!({ "state": "deleted" });
            scratch.write(
                &format!("{record}/outcome.json"),
                r#"{"state": "finished"}"#,
            );
        }
        scratch.write(&format!("{record}/agent/sandbox.json"), &sandbox);
    }
    // The image of a job that a client started as the controller came up, which no IR names yet.
    scratch.write("images", &format!("{}\n{}\n", image('a'), image('b')));
    send(&scratch, &id_ago(0, "0b000006"), UPSTREAMS, false);
    let mut boot = on_controller(&scratch, &["controller", "boot"]);
    // As its unit has it: systemd's `/run/user/%U` names root's, the system manager's user's.
    boot.env("XDG_RUNTIME_DIR", scratch.0.join("root-run"));
    let invoked = invocation(&boot);
    let mut lines = vec![report(invoked, &boot.output().unwrap(), &scratch.0, false)];
    lines.extend(noted(&scratch));
    for run in [&clean, &local, &lost] {
        let outcome = format!("home/.pumpkin/runs/{run}/outcome.json");
        let outcome = fs::read_to_string(scratch.0.join(outcome)).unwrap();
        lines.push(format!("{run}'s outcome.json: {outcome}"));
    }
    check(
        &scratch,
        &lines.join("\n"),
        &[
            (clean.as_str(), "<clean>"),
            (local.as_str(), "<local>"),
            (lost.as_str(), "<lost>"),
        ],
        &expect![[r#"
            $ HOME=<scratch>/home PATH=<scratch>/bin:/usr/bin:/bin XDG_RUNTIME_DIR=<scratch>/root-run pumpkin controller boot
            exit status: Some(0)
            pumpkin: run <local>'s sandbox sandbox-<local> on provider `local` had ended, as it isn't listed there now: what it left there is now removed
            pumpkin: run <local> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            pumpkin: run <lost>'s sandboxes on provider `aws`, if it left any, end by the run's hard end, as the boot can't reach it: `aws sts get-caller-identity` failed with exit status: 1: the stand-in aws answers only `--version` and `ssm get-parameter`: fix what `aws` reports
            pumpkin: run <lost> was lost, as its driver ended without an outcome.json: it is now recorded as failed
            printed:
              <local>
              <lost>
            aws --version
            aws --no-cli-pager --output json --region us-west-1 sts get-caller-identity
            <clean>'s outcome.json: {"state": "finished"}
            <local>'s outcome.json: {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }

            <lost>'s outcome.json: {
              "reason": "its driver ended without an outcome",
              "state": "failed"
            }"#]],
    );
}

const PINNED: &str = "1fe98e9dcf36c2dafe0df654e3899facc7dfa6f8";

/// The controller's instance, which carries the tag of controller `alice`.
const INSTANCE: &str = "i-0c00000000000a11c";

/// The shared connections of the client's workspace to controller `alice`, below the scratch
/// directory.
const SHARED: &str = "repo/.pumpkin/controller/alice.us-west-1";

/// The client's key, of its upstream `gateway`.
const KEY: &str = "sk-client-stand-in";

/// The stand-in `ssh` to the controller, of shared masters that answer: one that `-N` opens makes
/// its socket, unless `refused-<m>` is there for master `<m>`, which then fails as one whose
/// connection closes; `-O check` of one succeeds, but once after a cut over it; and any other
/// call notes its arguments in `argv`, a line each, and the first line of its command but a
/// `set`, in `scripts`, runs `before-<n>` beside it once, if it is there, before the `<n>`th call
/// that `scripts` notes, then runs its command as the controller's user, in `home`, with runtime
/// directory `run`. A receive over master `<m>` adds what it is sent to `received-<m>`, and is cut,
/// if `cut-<m>` is there, after as many bytes as it says, as a connection that ends.
const SSH: &str = r#"#!/bin/sh
dir=${0%/*}
printf '%s\n' "$@" >> "$dir/argv"
while :; do
    case $1 in
        -N) if [ -e "$dir/refused-$master" ]; then
                echo 'Connection closed by UNKNOWN port 65535' >> "$master.log"; exit 255
            fi
            (umask 077 && : > "$master"); exit 0 ;;
        -O) if rm "$dir/unanswered-$master" 2>/dev/null; then exit 255; fi; exit 0 ;;
        -o) case $2 in ControlPath=*) master=${2#ControlPath=} ;; esac; shift 2 ;;
        -F|-i|-E|-l) shift 2 ;;
        -*) shift ;;
        *) break ;;
    esac
done
shift
first=$(printf '%s\n' "$*" | sed --quiet '/^set /!{p;q;}')
printf '%s\n' "$first" >> "$dir/scripts"
calls=$(wc --lines < "$dir/scripts")
if [ -e "$dir/before-$calls" ]; then sh "$dir/before-$calls"; rm "$dir/before-$calls"; fi
cd "$dir/../home" || exit 255
export HOME="$PWD" XDG_RUNTIME_DIR="$dir/../run"
[ "$first" = 'cd "$HOME/store/incoming"' ] || exec sh -c "$*"
if [ -e "$dir/cut-$master" ]; then
    head --bytes="$(cat "$dir/cut-$master")" | sh -c "$*"
    rm "$dir/cut-$master"; touch "$dir/unanswered-$master"; exit 255
fi
tee --append "$dir/received-$master" | sh -c "$*"
"#;

/// The stand-in `systemd-run`, which notes its arguments in `argv` beside it, as [`SSH`] does,
/// and runs nothing.
const SYSTEMD_RUN: &str = "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"${0%/*}/argv\"\n";

/// Sets `scratch` up as the client and its controller: in `bin`, the stand-ins for `aws`
/// ([`AWS`]), `ssh` ([`SSH`]), `systemd-run` ([`SYSTEMD_RUN`]) and the Session Manager plugin;
/// the controller user's home, `home`, and runtime directory, `run`; the client's home, `client`,
/// whose configuration names controller `alice`, beside its upstream, whose key is [`KEY`]; and
/// its task's repository, `repo`, pinning Pumpkin [`PINNED`], with the `pumpkin` of it that
/// `./pumpkin` builds in the repository's workspace, and the bundle of it that it keeps, and the
/// task `stand-in`. Returns the client's environment.
fn set_up_client(scratch: &Scratch) -> [(String, String); 2] {
    for (name, script) in [
        ("aws", AWS),
        ("ssh", SSH),
        ("systemd-run", SYSTEMD_RUN),
        ("session-manager-plugin", ""),
    ] {
        scratch.write_executable(&format!("bin/{name}"), script.as_bytes());
    }
    for dir in ["home", "run"] {
        fs::create_dir_all(scratch.0.join(dir)).unwrap();
    }
    install(scratch, &format!("repo/.pumpkin/{PINNED}/bin"));
    let config = format!(
        "[controller]\nname = \"alice\"\nregion = \"us-west-1\"\n\n{}",
        UPSTREAMS.replace("\"gateway.key\"", "\"gateway-key\"")
    );
    scratch.write("client/.config/pumpkin/pumpkin.toml", &config);
    scratch.write("client/.config/pumpkin/gateway-key", KEY);
    // Not git's own variables, such as the `GIT_DIR` that `git rebase --exec` exports, which
    // would point `git init` at another repository.
    let git = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]))
        .args(["init", "--quiet", "repo"])
        .current_dir(&scratch.0)
        .status();
    assert!(git.unwrap().success());
    scratch.write_task_in("repo", "stand-in", &workflow(&image('0'), &["true"]));
    // The consumer entry, which the job would run, as the stand-in `systemd-run` runs nothing.
    scratch.write_executable("repo/pumpkin", b"#!/bin/sh\n");
    scratch.write(
        "repo/.pumpkinversion",
        &format!("controller-candidate {PINNED}\n"),
    );
    scratch.write("repo/.pumpkin/.gitignore", "*\n");
    // A file of the repository's that the task doesn't reach, as another task's data.
    scratch.write("repo/other/data", "another task's\n");
    scratch.write(
        &format!("repo/.pumpkin/{PINNED}/pumpkin.bundle"),
        "a bundle of Pumpkin\n",
    );
    let path = format!("{}:/usr/bin:/bin", scratch.0.join("bin").display());
    [
        (
            "HOME".to_owned(),
            scratch.0.join("client").display().to_string(),
        ),
        ("PATH".to_owned(), path),
    ]
}

/// `pumpkin` with `args`, as the client runs it, in its task's repository, as its `./pumpkin`
/// built it there.
fn on_client(scratch: &Scratch, environment: &[(String, String); 2], args: &[&str]) -> Command {
    let pumpkin = scratch
        .0
        .join(format!("repo/.pumpkin/{PINNED}/bin/pumpkin"));
    let mut command = scratch.program(pumpkin, args);
    command
        .current_dir(scratch.0.join("repo"))
        .envs(environment.iter().map(|(name, value)| (name, value)));
    command
}

/// The files below `dir`, each with its mode, sorted; or that there is none.
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

/// `dir` with its mode, then [`modes`] of what it holds.
fn mode_and_modes(dir: &Path) -> String {
    let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
    format!("{mode:o}: {}", modes(dir))
}

/// Sets `scratch` up as [`set_up_client`] does, then has the client start a job of the task
/// `stand-in` on its controller, over a shared master that answers, as a later command finds one
/// that an earlier opened: its first command makes this machine's key and the workspace's shared
/// connections, and finds no controller, as the stand-in `aws` describes none; the test then
/// notes the controller's instance and the master's socket there, and `pumpkin run` sends the
/// task. Returns the client's environment, the job's id and how the run ended.
fn started_job(scratch: &Scratch) -> ([(String, String); 2], String, String) {
    let client = set_up_client(scratch);
    let (id, ran) = start(scratch, &client);
    (client, id, ran)
}

/// Has the client of `client`'s environment, which [`set_up_client`] set up, start a job of the
/// task `stand-in`, as [`started_job`] says. Returns the job's id, if it started, and how the run
/// ended, with the id as `<id>`.
fn start(scratch: &Scratch, client: &[(String, String); 2]) -> (String, String) {
    let socket = scratch.0.join(SHARED).join("0");
    if !socket.exists() {
        let first = on_client(scratch, client, &["ls"]).output().unwrap();
        assert!(!first.status.success(), "{first:?}");
        fs::write(scratch.0.join(SHARED).join("lock"), INSTANCE).unwrap();
        // As `ssh` makes a master's socket, which only its user may reach.
        fs::write(&socket, "").unwrap();
        fs::set_permissions(&socket, Permissions::from_mode(0o600)).unwrap();
    }
    let mut run = on_client(scratch, client, &["run", "stand-in/task.star"]);
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

#[test]
fn keys_on_standard_input() {
    let scratch = Scratch::new("controller-keys");
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
            argv.contains("set -o errexit")
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
            pumpkin: controller `alice` lacks all 5 of the task's files and Pumpkin's bundle: sending 1.9 KiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            an argument holds the key: false
            ssh ran on the controller: true
            the job's directory holds the key: true
            files in the workspace that hold the key: none"#]],
    );
}

// None that another user may read, but a public key.
#[test]
fn modes_of_what_the_client_keeps() {
    let scratch = Scratch::new("controller-modes");
    let (_, id, _) = started_job(&scratch);
    let lines = [
        format!(
            "this machine's key: {}",
            mode_and_modes(&scratch.0.join("client/.config/pumpkin/ssh"))
        ),
        format!(
            "the shared connections: {}",
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
            the shared connections: 700: 0 600, 0-0 600, lock 600, upload 600
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
    let scratch = Scratch::new("controller-target-refusals");
    let client = set_up_client(&scratch);
    let mut lines = Vec::new();
    let config = scratch.0.join("client/.config/pumpkin/pumpkin.toml");
    let both = format!(
        "{}\n[providers.local.docker]\n",
        fs::read_to_string(&config).unwrap()
    );
    scratch.write("both.toml", &both);
    let mut refused = on_client(
        &scratch,
        &client,
        &["run", "stand-in/task.star", "--config", "../both.toml"],
    );
    let invoked = invocation(&refused);
    lines.push(report(
        invoked,
        &refused.output().unwrap(),
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
    let mut outside = on_client(&scratch, &client, &["run", "stand-in/task.star"]);
    let invoked = invocation(&outside);
    lines.push(report(
        invoked,
        &outside.output().unwrap(),
        &scratch.0,
        false,
    ));
    fs::rename(&above, &marker).unwrap();
    let bundle = scratch
        .0
        .join(format!("repo/.pumpkin/{PINNED}/pumpkin.bundle"));
    fs::remove_file(bundle).unwrap();
    let mut unbundled = on_client(&scratch, &client, &["run", "stand-in/task.star"]);
    let invoked = invocation(&unbundled);
    lines.push(report(
        invoked,
        &unbundled.output().unwrap(),
        &scratch.0,
        false,
    ));
    fs::remove_file(scratch.0.join("bin/session-manager-plugin")).unwrap();
    let mut unplugged = on_client(&scratch, &client, &["ls"]);
    let invoked = invocation(&unplugged);
    lines.push(report(
        invoked,
        &unplugged.output().unwrap(),
        &scratch.0,
        false,
    ));
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
        pumpkin: ../both.toml: it names `[controller]` beside `[providers]`: a machine's runs go either to its controller or to its own providers, so keep `[controller]`, beside `[upstreams]`, or remove it
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star --runs runs
        exit status: Some(1)
        pumpkin: `--runs` names this machine's, but its configuration sends runs to controller `alice`: leave it out
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(1)
        pumpkin: stand-in/task.star: the task's root, <scratch>, is above its git repository, <scratch>/repo, which is all that a run on the controller is sent: put the root's `PUMPKIN.star` in the repository
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(1)
        pumpkin: <scratch>/repo/.pumpkin/1fe98e9dcf36c2dafe0df654e3899facc7dfa6f8/pumpkin.bundle isn't there, which the controller builds Pumpkin 1fe98e9dcf36c2dafe0df654e3899facc7dfa6f8 from: build it here with `./pumpkin --version`, after removing <scratch>/repo/.pumpkin/1fe98e9dcf36c2dafe0df654e3899facc7dfa6f8 if it was built without one
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin ls
        exit status: Some(1)
        pumpkin: the Session Manager plugin, `session-manager-plugin`, isn't on PATH, and `aws ssm start-session` needs it to reach the controller: install it, as AWS's Session Manager docs say
        calls of aws and ssh: 0"#]],
    );
}

/// The lowercase hexadecimal digits of the SHA-256 of `bytes`.
fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = <[u8; 32]>::from(sha2::Sha256::digest(bytes));
    Sha256Digest::from(digest).hex().to_owned()
}

/// The paths below `dir`, of files and symbolic links, relative to it, sorted, but those below a
/// directory named one of `skipped`.
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

/// The task's files, of the repository that [`set_up_client`] made, each by the SHA-256 of what it
/// holds, a symbolic link's target for a link, named: as `named` names it, else by the files of
/// the repository that hold the same; and Pumpkin's bundle, and the tree of no files.
fn holders(scratch: &Scratch, named: &[(String, &str)]) -> Vec<(String, String)> {
    let repo = scratch.0.join("repo");
    let mut holders: Vec<(String, String)> = (named.iter())
        .map(|(hex, name)| (hex.clone(), (*name).to_owned()))
        .collect();
    holders.push((EMPTY_TREE.to_owned(), "a tree of no files".to_owned()));
    let bundle = fs::read(repo.join(format!(".pumpkin/{PINNED}/pumpkin.bundle"))).unwrap();
    holders.push((sha256(&bundle), "Pumpkin's bundle".to_owned()));
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

/// What the controller's store holds, a line each: each blob by the files that `holders` gives
/// it, with its mode and its count of links, a copy of one at 0555 as such; and its manifests,
/// and the files of its `incoming/`. Asserts that each blob and manifest is named by its SHA-256.
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
        assert_eq!(sha256(&fs::read(store.join(&path)).unwrap()), hex, "{name}");
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
    for path in walked(&store.join("manifests"), &[]) {
        let name = path.to_str().unwrap().to_owned();
        let text = fs::read(store.join("manifests").join(&path)).unwrap();
        assert_eq!(sha256(&text), name);
        lines.push(format!("  a manifest: {}", named(&name)));
    }
    let incoming = walked(&store.join("incoming"), &[]);
    if incoming.is_empty() {
        lines.push("  incoming/: none".to_owned());
    }
    for path in incoming {
        let name = path.to_str().unwrap();
        let size_bytes = fs::metadata(store.join("incoming").join(&path))
            .unwrap()
            .len();
        let (hex, index) = name.split_once('.').unwrap();
        let piece = format!("piece {index} of {}", named(hex));
        lines.push(format!("  incoming/: {piece}, {size_bytes} bytes"));
    }
    lines
}

/// The scripts that the stand-in `ssh` ran since this was last called, each by what it does, as
/// its first line, which [`SSH`] notes, says.
fn calls(scratch: &Scratch) -> String {
    let path = scratch.0.join("bin/scripts");
    let text = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(path);
    let calls = text.lines().map(|line| match line {
        r#"store="$HOME/store""# => "query",
        r#"cd "$HOME/store/incoming""# => "receive",
        line if line.starts_with("sent=") => "start",
        line => line,
    });
    let calls: Vec<&str> = calls.collect();
    if calls.is_empty() {
        return "the calls: none".to_owned();
    }
    format!("the calls: {}", calls.join(", "))
}

/// The tree's SHA-256 that the last job that the stand-in `systemd-run` noted was started with.
fn last_tree(scratch: &Scratch) -> String {
    let argv = fs::read_to_string(scratch.0.join("bin/argv")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    let at = lines.iter().rposition(|line| *line == "--tree").unwrap();
    lines[at + 1].to_owned()
}

/// `controller job` of `id`, of tree `tree`, as its unit runs it, in its tree, building the task
/// `stand-in`: its output.
fn job_of(scratch: &Scratch, id: &str, tree: &str) -> std::process::Output {
    let mut job = on_controller(
        scratch,
        &[
            "controller",
            "job",
            id,
            "--region",
            "us-west-1",
            "--parameter",
            "/pumpkin-alice/settings",
            "--tree",
            tree,
            "--",
            "build",
            "stand-in/task.star",
        ],
    );
    job.current_dir(scratch.0.join(format!("home/trees/repo/{id}")));
    job.output().unwrap()
}

/// Writes into the task `stand-in` of the client's repository a file its owner may execute, a
/// symbolic link, a name with a space, an empty file, and one that git lists, as it is in its
/// index, but that is gone.
fn write_files(scratch: &Scratch) {
    scratch.write_executable("repo/stand-in/run.sh", b"#!/bin/sh\necho run\n");
    symlink("Dockerfile", scratch.0.join("repo/stand-in/CLAUDE.md")).unwrap();
    scratch.write("repo/stand-in/with space.txt", "spaced\n");
    scratch.write("repo/stand-in/empty", "");
    scratch.write("repo/stand-in/gone", "");
    let added = stdx::process::command("git", Environment::Only(&["HOME", "PATH"]))
        .args(["-C", "repo", "add", "stand-in/gone"])
        .current_dir(&scratch.0)
        .status();
    assert!(added.unwrap().success());
    fs::remove_file(scratch.0.join("repo/stand-in/gone")).unwrap();
}

/// The files of the job's tree at `dir`, a line each: each as a link of its blob in the store,
/// or of the blob's copy to run, with its mode, or a symbolic link with its target.
fn tree_files(scratch: &Scratch, dir: &Path) -> Vec<String> {
    let store = scratch.0.join("home/store");
    let mut lines = vec!["the job's tree:".to_owned()];
    for path in walked(dir, &[".pumpkin"]) {
        let at = dir.join(&path);
        let metadata = fs::symlink_metadata(&at).unwrap();
        let line = if metadata.is_symlink() {
            format!(
                "a symbolic link to {}",
                fs::read_link(&at).unwrap().display()
            )
        } else {
            let hex = sha256(&fs::read(&at).unwrap());
            let linked = |name: String| {
                let blob = fs::metadata(store.join(name));
                blob.is_ok_and(|blob| blob.ino() == metadata.ino())
            };
            let of = if linked(hex.clone()) {
                "its blob"
            } else if linked(format!("{hex}.x")) {
                "its blob's copy to run"
            } else {
                "nothing of the store's"
            };
            let mode = metadata.permissions().mode() & 0o7777;
            format!("a link of {of}, {mode:o}")
        };
        lines.push(format!("  {}: {line}", path.display()));
    }
    lines
}

// The job's build refuses the link in its context, once it has made the tree.
#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn send_by_content() {
    let scratch = Scratch::new("controller-send");
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let client = set_up_client(&scratch);
    write_files(&scratch);
    let (id, first) = start(&scratch, &client);
    let mut lines = vec![first, calls(&scratch)];
    let tree = [(last_tree(&scratch), "the task's tree")];
    lines.extend(stored(&scratch, &holders(&scratch, &tree)));
    job_of(&scratch, &id, &last_tree(&scratch));
    let dir = scratch.0.join(format!("home/trees/repo/{id}"));
    lines.extend(tree_files(&scratch, &dir));
    let (_, again) = start(&scratch, &client);
    lines.extend([again, calls(&scratch)]);
    scratch.write("repo/stand-in/with space.txt", "spaced again\n");
    let (_, changed) = start(&scratch, &client);
    lines.extend([changed, calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks all 9 of the task's files and Pumpkin's bundle: sending 2.0 KiB
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, start
        the store:
          .pumpkinversion: 444, 2 links
          PUMPKIN.star: 444, 1 link
          Pumpkin's bundle: 444, 2 links
          pumpkin, to run: 555, 2 links
          pumpkin: 444, 1 link
          stand-in/CLAUDE.md: 444, 1 link
          stand-in/Dockerfile: 444, 1 link
          stand-in/empty: 444, 1 link
          stand-in/run.sh: 444, 1 link
          stand-in/task.star: 444, 1 link
          stand-in/with space.txt: 444, 1 link
          a manifest: the task's tree
          a manifest: a tree of no files
          incoming/: none
        the job's tree:
          .pumpkin: a symbolic link to <scratch>/home/.pumpkin
          .pumpkinversion: a link of its blob, 444
          PUMPKIN.star: a link of its blob, 444
          pumpkin: a link of its blob's copy to run, 555
          stand-in/CLAUDE.md: a symbolic link to Dockerfile
          stand-in/Dockerfile: a link of its blob, 444
          stand-in/empty: a link of its blob, 444
          stand-in/run.sh: a link of its blob's copy to run, 555
          stand-in/task.star: a link of its blob, 444
          stand-in/with space.txt: a link of its blob, 444
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` holds all 9 of the task's files
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, start
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks 1 of the task's 9 files: sending 13 bytes
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, start"#]],
    );
}

#[test]
fn send_refused_piece() {
    let scratch = Scratch::new("controller-send-refused");
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    // A piece of it, whole as its size goes, left with other bytes, as by a file changed meanwhile.
    let hex = sha256(b"noted\n");
    scratch.write(&format!("home/store/incoming/{hex}.00000"), "Noted\n");
    let (_, refused) = start(&scratch, &client);
    let mut lines = vec![refused, calls(&scratch)];
    let store = scratch.0.join("home/store");
    lines.push(format!(
        "the store holds it: {}; a piece of it: {}",
        store.join(&hex).exists(),
        store.join(format!("incoming/{hex}.00000")).exists()
    ));
    let (_, again) = start(&scratch, &client);
    lines.extend([again, calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(1)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks 5 of the task's 6 files and Pumpkin's bundle: sending 1.9 KiB
        pumpkin: <scratch>/repo/stand-in/notes changed while it was sent: run it again
        the calls: query, query, receive, start
        the store holds it: false; a piece of it: false
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks 1 of the task's 6 files: sending 6 bytes
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, start"#]],
    );
}

/// Writes the file `name` of the client's task, of `size_bytes`, each its index's remainder by
/// `modulus`.
fn write_big(scratch: &Scratch, name: &str, size_bytes: u32, modulus: u32) {
    let big: Vec<u8> = (0..size_bytes)
        .map(|index| u8::try_from(index % modulus).unwrap())
        .collect();
    fs::write(scratch.0.join("repo/stand-in").join(name), &big).unwrap();
}

/// What each receive over a shared master was sent, a line each: by master, each member of the
/// `tar` stream by the file that `holders` gives its blob, with its piece's index and its bytes.
fn received(scratch: &Scratch, holders: &[(String, String)]) -> Vec<String> {
    let mut lines = Vec::new();
    for master in 0..16 {
        let Ok(stream) = fs::read(scratch.0.join(format!("bin/received-{master}"))) else {
            continue;
        };
        let (mut members, mut at, mut total_bytes) = (Vec::new(), 0, 0);
        while stream[at] != 0 {
            let header = &stream[at..at + 512];
            let name = String::from_utf8(header[..100].to_vec()).unwrap();
            let (hex, index) = name.trim_end_matches('\0').split_once('.').unwrap();
            let size = std::str::from_utf8(&header[124..135]).unwrap();
            let size_bytes = u64::from_str_radix(size, 8).unwrap();
            let file = holders.iter().find(|(held, _)| held == hex).unwrap();
            members.push(format!("{} {index} ({size_bytes})", file.1));
            total_bytes += size_bytes;
            at += 512 + usize::try_from(size_bytes.next_multiple_of(512)).unwrap();
        }
        lines.push(format!(
            "over {master}, {total_bytes} bytes: {}",
            members.join(", ")
        ));
    }
    lines
}

// The connection that carries the first piece of a file of 17 MiB ends part way through it, while
// the other, of its second piece and the rest, ends whole.
#[test]
fn send_carries_on() {
    let scratch = Scratch::new("controller-send-carries-on");
    let client = set_up_client(&scratch);
    write_big(&scratch, "big.bin", 17 << 20, 251);
    scratch.write("bin/cut-0", &(64 << 10).to_string());
    let (_, sent) = start(&scratch, &client);
    let mut lines = vec![sent, calls(&scratch)];
    let holders = holders(&scratch, &[]);
    lines.extend(received(&scratch, &holders));
    let stored = stored(&scratch, &holders);
    let shown = ["big.bin", "incoming"];
    lines.extend(
        stored
            .into_iter()
            .filter(|line| shown.iter().any(|name| line.contains(name))),
    );
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
            $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
            exit status: Some(0)
            pumpkin: run <id> goes to controller `alice`
            pumpkin: controller `alice` lacks all 6 of the task's files and Pumpkin's bundle: sending 17.0 MiB over 2 connections
            tar: Unexpected EOF in archive
            tar: Unexpected EOF in archive
            tar: Error is not recoverable: exiting now
            pumpkin: the connection to controller `alice` ended, as its log says: <scratch>/repo/.pumpkin/controller/alice.us-west-1/0.log: asking again what it lacks, now, attempt 2 of 7
            pumpkin: controller `alice` lacks 1 of the task's 6 files: sending 16.0 MiB
            pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
            pumpkin: to follow it: pumpkin wait <id>
            pumpkin: to stop it: pumpkin cancel <id>
            printed:
              <id>
            the calls: query, query, receive, receive, query, receive, start
            over 0, 16777216 bytes: stand-in/big.bin 00000 (16777216)
            over 1, 1050617 bytes: stand-in/big.bin 00001 (1048576), stand-in/task.star 00000 (1806), stand-in/Dockerfile 00000 (118), .pumpkinversion 00000 (62), PUMPKIN.star 00000 (25), Pumpkin's bundle 00000 (20), pumpkin 00000 (10)
              stand-in/big.bin: 444, 1 link
              incoming/: none"#]],
    );
}

// Files of 20, 8 and 5 MiB, with the task's few small ones, are 3 streams' worth.
#[test]
fn send_over_streams() {
    let scratch = Scratch::new("controller-send-streams");
    let client = set_up_client(&scratch);
    let files = [
        ("a.bin", 20 << 20, 251),
        ("b.bin", 8 << 20, 241),
        ("c.bin", 5 << 20, 239),
    ];
    for (name, size_bytes, modulus) in files {
        write_big(&scratch, name, size_bytes, modulus);
    }
    let (_, sent) = start(&scratch, &client);
    let mut lines = vec![sent, calls(&scratch)];
    let holders = holders(&scratch, &[(last_tree(&scratch), "the task's tree")]);
    lines.extend(received(&scratch, &holders));
    lines.extend(stored(&scratch, &holders));
    lines.push(format!(
        "the connections: {}",
        modes(&scratch.0.join(SHARED))
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks all 8 of the task's files and Pumpkin's bundle: sending 33.0 MiB over 3 connections
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, receive, receive, start
        over 0, 16777216 bytes: stand-in/a.bin 00000 (16777216)
        over 1, 8390649 bytes: stand-in/b.bin 00000 (8388608), stand-in/task.star 00000 (1806), stand-in/Dockerfile 00000 (118), .pumpkinversion 00000 (62), PUMPKIN.star 00000 (25), Pumpkin's bundle 00000 (20), pumpkin 00000 (10)
        over 2, 9437184 bytes: stand-in/c.bin 00000 (5242880), stand-in/a.bin 00001 (4194304)
        the store:
          .pumpkinversion: 444, 2 links
          PUMPKIN.star: 444, 1 link
          Pumpkin's bundle: 444, 2 links
          pumpkin, to run: 555, 2 links
          pumpkin: 444, 1 link
          stand-in/Dockerfile: 444, 1 link
          stand-in/a.bin: 444, 1 link
          stand-in/b.bin: 444, 1 link
          stand-in/c.bin: 444, 1 link
          stand-in/task.star: 444, 1 link
          a manifest: the task's tree
          incoming/: none
        the connections: 0 600, 0-0 600, 1 600, 1-0 600, 1.log 600, 2 600, 2-0 600, 2.log 600, lock 600, upload 600"#]],
    );
}

// Files of 17 MiB, 2 streams' worth, while every session of masters 1 to 15 is held, as by
// other commands: both streams go over master 0.
#[test]
fn send_over_one_connection() {
    let scratch = Scratch::new("controller-send-one-connection");
    let client = set_up_client(&scratch);
    write_big(&scratch, "big.bin", 17 << 20, 251);
    let shared = scratch.0.join(SHARED);
    fs::create_dir_all(&shared).unwrap();
    fs::set_permissions(&shared, Permissions::from_mode(0o700)).unwrap();
    let mut held = Vec::new();
    for master in 1..16 {
        fs::write(shared.join(master.to_string()), "").unwrap();
        for session in 0..5 {
            let slot = File::create(shared.join(format!("{master}-{session}"))).unwrap();
            slot.lock().unwrap();
            held.push(slot);
        }
    }
    let (_, sent) = start(&scratch, &client);
    drop(held);
    let mut lines = vec![sent, calls(&scratch)];
    let holders = holders(&scratch, &[(last_tree(&scratch), "the task's tree")]);
    lines.extend(stored(&scratch, &holders));
    let sessions = modes(&shared);
    let sessions = sessions.split(", ").filter(|file| file.starts_with('0'));
    lines.push(format!(
        "master 0: {}",
        sessions.collect::<Vec<_>>().join(", ")
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks all 6 of the task's files and Pumpkin's bundle: sending 17.0 MiB over 2 connections
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, receive, start
        the store:
          .pumpkinversion: 444, 2 links
          PUMPKIN.star: 444, 1 link
          Pumpkin's bundle: 444, 2 links
          pumpkin, to run: 555, 2 links
          pumpkin: 444, 1 link
          stand-in/Dockerfile: 444, 1 link
          stand-in/big.bin: 444, 1 link
          stand-in/task.star: 444, 1 link
          a manifest: the task's tree
          incoming/: none
        master 0: 0 600, 0-0 600, 0-1 600"#]],
    );
}

// Files of 17 MiB, 2 streams' worth, while master 1 can't be opened: both go over master 0.
#[test]
fn send_over_the_connection_held() {
    let scratch = Scratch::new("controller-send-connection-held");
    let client = set_up_client(&scratch);
    write_big(&scratch, "big.bin", 17 << 20, 251);
    scratch.write("bin/refused-1", "");
    let (_, sent) = start(&scratch, &client);
    let mut lines = vec![sent, calls(&scratch)];
    let holders = holders(&scratch, &[(last_tree(&scratch), "the task's tree")]);
    lines.extend(stored(&scratch, &holders));
    lines.push(format!(
        "the connections: {}",
        modes(&scratch.0.join(SHARED))
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks all 6 of the task's files and Pumpkin's bundle: sending 17.0 MiB over 2 connections
        pumpkin: connection 1 to controller `alice` can't be opened, as its log says: <scratch>/repo/.pumpkin/controller/alice.us-west-1/1.log: this goes on over those it holds
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, receive, start
        the store:
          .pumpkinversion: 444, 2 links
          PUMPKIN.star: 444, 1 link
          Pumpkin's bundle: 444, 2 links
          pumpkin, to run: 555, 2 links
          pumpkin: 444, 1 link
          stand-in/Dockerfile: 444, 1 link
          stand-in/big.bin: 444, 1 link
          stand-in/task.star: 444, 1 link
          a manifest: the task's tree
          incoming/: none
        the connections: 0 600, 0-0 600, 0-1 600, 1.log 600, lock 600, upload 600"#]],
    );
}

#[test]
fn send_refusals() {
    let scratch = Scratch::new("controller-send-refusals");
    let client = set_up_client(&scratch);
    let mut lines = Vec::new();
    for name in [&b"repo/stand-in/a\nb"[..], b"repo/stand-in/\xff"] {
        let path = scratch.0.join(OsStr::from_bytes(name));
        fs::write(&path, "").unwrap();
        let (_, refused) = start(&scratch, &client);
        lines.extend([refused, calls(&scratch)]);
        fs::remove_file(path).unwrap();
    }
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
        the calls: none"#]],
    );
}

#[test]
fn send_changed_file() {
    let scratch = Scratch::new("controller-send-changed");
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    // Shortened once it is hashed, as the first call to the controller begins.
    let notes = scratch.0.join("repo/stand-in/notes");
    scratch.write(
        "bin/before-1",
        &format!("printf no > '{}'\n", notes.display()),
    );
    let (_, changed) = start(&scratch, &client);
    let mut lines = vec![changed, calls(&scratch)];
    let (_, again) = start(&scratch, &client);
    lines.extend([again, calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(1)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks all 6 of the task's files and Pumpkin's bundle: sending 1.9 KiB
        pumpkin: <scratch>/repo/stand-in/notes changed while it was sent: run it again
        the calls: query, query, receive
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks 1 of the task's 6 files: sending 2 bytes
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, start"#]],
    );
}

// A new bundle of the same commit replaces the workspace's; then the prune, as it were, removes
// a file and the bundle that the controller said it holds before the start takes them.
#[test]
fn send_bundle_and_gone() {
    let scratch = Scratch::new("controller-send-gone");
    let client = set_up_client(&scratch);
    scratch.write("repo/stand-in/notes", "noted\n");
    let (_, first) = start(&scratch, &client);
    assert!(first.contains("exit status: Some(0)"), "{first}");
    calls(&scratch);
    let bundle = format!("repo/.pumpkin/{PINNED}/pumpkin.bundle");
    scratch.write(&bundle, "another bundle of Pumpkin\n");
    let (_, rebundled) = start(&scratch, &client);
    let mut lines = vec![rebundled, calls(&scratch)];
    let kept = fs::read_to_string(scratch.0.join(format!("home/.pumpkin/{PINNED}.bundle")));
    lines.push(format!("the workspace's bundle: {:?}", kept.unwrap()));
    let store = scratch.0.join("home/store");
    let removed = [sha256(b"noted\n"), sha256(b"another bundle of Pumpkin\n")];
    let removed = removed.map(|hex| format!("rm '{}'\n", store.join(hex).display()));
    scratch.write("bin/before-2", &removed.concat());
    let (_, gone) = start(&scratch, &client);
    lines.extend([gone, calls(&scratch)]);
    check(
        &scratch,
        &lines.join("\n"),
        &[],
        &expect![[r#"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(0)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` lacks Pumpkin's bundle: sending 26 bytes
        pumpkin: run <id> is a job on controller `alice`, which builds it, then runs it
        pumpkin: to follow it: pumpkin wait <id>
        pumpkin: to stop it: pumpkin cancel <id>
        printed:
          <id>
        the calls: query, query, receive, start
        the workspace's bundle: "another bundle of Pumpkin\n"
        $ HOME=<scratch>/client PATH=<scratch>/bin:/usr/bin:/bin pumpkin run stand-in/task.star
        exit status: Some(1)
        pumpkin: run <id> goes to controller `alice`
        pumpkin: controller `alice` holds all 6 of the task's files
        pumpkin: 1 of the task's files and Pumpkin's bundle are no longer whole on the controller: run it again
        the calls: query, start"#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn store_collection() {
    let scratch = Scratch::new("controller-store-collection");
    let _stand_in = StandIn::new(&scratch);
    set_up(&scratch);
    let client = set_up_client(&scratch);
    let mut named = Vec::new();
    let mut trees = Vec::new();
    for notes in ["first", "second", "third"] {
        scratch.write("repo/stand-in/notes", notes);
        let (id, ran) = start(&scratch, &client);
        assert!(ran.contains("exit status: Some(0)"), "{ran}");
        named.push((
            sha256(notes.as_bytes()),
            format!("stand-in/notes, as the {notes} sent it"),
        ));
        named.push((
            last_tree(&scratch),
            format!("the tree that the {notes} sent"),
        ));
        trees.push((id, last_tree(&scratch)));
    }
    // The first's job makes its tree, which links its files; the second's never starts.
    let (first, tree) = &trees[0];
    assert!(job_of(&scratch, first, tree).status.success());
    let two_days_ago = SystemTime::now() - Duration::from_hours(48);
    for (_, tree) in &trees[..2] {
        let manifest = scratch.0.join(format!("home/store/manifests/{tree}"));
        File::open(manifest)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    }
    for (bytes, when) in [
        ("of two days ago", two_days_ago),
        ("of now", SystemTime::now()),
    ] {
        let hex = sha256(bytes.as_bytes());
        let piece = format!("home/store/incoming/{hex}.00000");
        scratch.write(&piece, bytes);
        File::open(scratch.0.join(piece))
            .unwrap()
            .set_modified(when)
            .unwrap();
        named.push((hex, format!("a blob {bytes}")));
    }
    // Of a later Pumpkin's, say, which may name any blob.
    let foreign = "pumpkin-tree/2\nwhat a later form holds\n";
    let foreign_hex = sha256(foreign.as_bytes());
    let planted = format!("home/store/manifests/{foreign_hex}");
    scratch.write(&planted, foreign);
    named.push((foreign_hex.clone(), "a manifest of another form".to_owned()));
    let (third, tree) = &trees[2];
    let built = job_of(&scratch, third, tree);
    assert!(built.status.success(), "{built:?}");
    let named: Vec<(String, &str)> = (named.iter())
        .map(|(hex, name)| (hex.clone(), name.as_str()))
        .collect();
    let said = String::from_utf8(built.stderr).unwrap();
    let mut lines: Vec<String> = (said.lines())
        .filter(|line| line.contains("keeps every blob"))
        .map(str::to_owned)
        .collect();
    let shown = ["second", "manifest", "incoming"];
    lines.extend(
        (stored(&scratch, &holders(&scratch, &named)).into_iter())
            .filter(|line| shown.iter().any(|name| line.contains(name))),
    );
    // The third's again, once the manifest of another form is gone.
    fs::remove_file(scratch.0.join(planted)).unwrap();
    let (fourth, ran) = start(&scratch, &client);
    assert!(ran.contains("exit status: Some(0)"), "{ran}");
    let built = job_of(&scratch, &fourth, tree);
    assert!(built.status.success(), "{built:?}");
    lines.extend(stored(&scratch, &holders(&scratch, &named)));
    let config = scratch.0.join(format!("home/builds/{fourth}/pumpkin.toml"));
    let mut here = on_controller(
        &scratch,
        &[
            "build",
            "--config",
            config.to_str().unwrap(),
            "stand-in/task.star",
        ],
    );
    let here = here.current_dir(scratch.0.join("repo")).output().unwrap();
    assert!(here.status.success(), "{here:?}");
    lines.push(format!(
        "the job built the IR that the repository builds: {}",
        built.stdout == here.stdout
    ));
    check(
        &scratch,
        &lines.join("\n"),
        &[(&foreign_hex, "<another form>")],
        &expect![[r#"
            pumpkin: <scratch>/home/store/manifests/<another form>: the manifest doesn't start with `pumpkin-tree/1`: expected a manifest of this Pumpkin's form; so the prune keeps every blob of the store, which it may name
              stand-in/notes, as the second sent it: 444, 1 link
              a manifest: the tree that the third sent
              a manifest: a tree of no files
              a manifest: a manifest of another form
              incoming/: piece 00000 of a blob of now, 6 bytes
            the store:
              .pumpkinversion: 444, 5 links
              PUMPKIN.star: 444, 4 links
              Pumpkin's bundle: 444, 2 links
              pumpkin, to run: 555, 5 links
              pumpkin: 444, 1 link
              stand-in/Dockerfile: 444, 4 links
              stand-in/notes, as the first sent it: 444, 2 links
              stand-in/notes, as the third sent it: 444, 3 links
              stand-in/task.star: 444, 4 links
              a manifest: the tree that the third sent
              a manifest: a tree of no files
              incoming/: piece 00000 of a blob of now, 6 bytes
            the job built the IR that the repository builds: true"#]],
    );
}
