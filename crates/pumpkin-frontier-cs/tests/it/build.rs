//! Building a ported task's images with `pumpkin-frontier-cs build`, on a task whose recipes
//! copy a file each into an empty image, and with Codex's files served to the build from a
//! fixture in place of its download, so that they build at once, fetching at most the pinned
//! image of the stage that sets their modes, which the Docker provider's tests use too; and what
//! the command refuses before it reaches Docker. The build needs Docker, so it is ignored, as
//! the Docker provider's tests are: `cargo xtask ci --docker` runs it, through the context
//! `PUMPKIN_DOCKER_CONTEXT` names, `default` if it is unset.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(
    clippy::print_stderr,
    reason = "a test that can't remove an image's tag says so"
)]

use std::fs;
use std::io::Write as _;
use std::iter;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use expect_test::{Expect, expect};
use pumpkin_harness::codex::CODEX_0_167_0;
use serde_json::Value;
use stdx::process::Environment;

/// The Docker context the tests build through.
fn context() -> String {
    std::env::var("PUMPKIN_DOCKER_CONTEXT").unwrap_or_else(|_| "default".to_owned())
}

/// `docker`, run as the provider runs it, against the tests' context alone.
// Repeated in `providers/docker/tests/it.rs` and `crates/pumpkin/tests/it/docker.rs`: change
// all three.
fn docker() -> Command {
    // Only what the provider's `docker` keeps of the environment, for the reasons it gives.
    let kept = Environment::Only(&["HOME", "PATH", "DOCKER_CONFIG"]);
    let mut docker = stdx::process::command("docker", kept);
    docker.arg(format!("--context={}", context()));
    docker
}

/// Writes `text` to the file at `path` in `dir`, with permission bits `mode`, making the
/// directories it needs with bits 0755, whatever this process's umask.
fn put(dir: &Path, path: &str, text: &[u8], mode: u32) {
    let file = dir.join(path);
    let parent = file.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    for dir in [dir, parent] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(&file, text).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
}

/// A run of `pumpkin-frontier-cs build` on a test's task, with a stand-in for `docker` first on
/// its `PATH`, which serves the build that adds Codex the files Codex's recipe unpacks, from the
/// test's fixture, by a context named for the stage that unpacks them, and otherwise runs
/// `docker` as `PATH` finds it after the stand-in.
#[derive(Clone, Copy, Debug)]
enum Run {
    /// With nothing more.
    Plain,
    /// After the agent image's tag is moved to an image of the test's own, which nothing else
    /// tags, with a stand-in that finds no image of the task's, as when another build of the
    /// same inputs tags its image between this one's lookup and its tag.
    Raced,
    /// After the task's `notes` changes, with a stand-in that changes it back as each build
    /// starts.
    Edited,
    /// With a stand-in that, as the command reaches the engine, once it has read the task,
    /// rewrites the agent's recipe into one the check refuses, which would fetch a URL.
    Swapped,
}

impl Run {
    /// The arms of the shell `case` by which the run's stand-in for `docker` answers what it
    /// stands in for, given the task's directory and the prefix of its images' tags.
    fn stand_in(self, dir: &Path, prefix: &str) -> String {
        match self {
            Self::Plain => String::new(),
            Self::Raced => {
                let missing = r#"echo "Error response from daemon: No such image" >&2; exit 1"#;
                format!(r#"*" image inspect "*" -- {prefix}"*) {missing};;"#)
            }
            Self::Edited => {
                let notes = dir.join("notes");
                format!(r#"*" build "*) printf one > '{}';;"#, notes.display())
            }
            Self::Swapped => {
                let recipe = dir.join("agent-base/Dockerfile");
                let text = r"FROM scratch\nADD http://localhost:1/notes /\n";
                format!(
                    r#"*" context inspect "*) printf '{text}' > '{}';;"#,
                    recipe.display()
                )
            }
        }
    }
}

/// Makes a task named `task`, whose agent recipe copies its `notes` and whose verifier recipe
/// copies itself, each into an empty image, runs `pumpkin-frontier-cs build` on it through
/// `context` as each of `runs` says, and compares with `expect` what each run shows, as
/// [`run_build`] gives it; then what the runs left in their temporary directory, the files of
/// the agent's image, and the tags each image has for the task, the agent's base's too. Removes
/// each such tag afterwards, and so each image no other task tags.
fn check(task: &str, context: &str, runs: &[Run], expect: &Expect) {
    let scratch =
        std::env::temp_dir().join(format!("pumpkin-frontier-cs-{}-{task}", std::process::id()));
    let dir = scratch.join(task);
    put(&dir, "notes", b"one", 0o644);
    let (agent, verifier) = ("COPY notes /", "COPY verifier/Dockerfile /");
    for (recipe, step) in [
        ("agent-base/Dockerfile", agent),
        ("verifier/Dockerfile", verifier),
    ] {
        put(
            &dir,
            recipe,
            format!("FROM scratch\n{step}\n").as_bytes(),
            0o644,
        );
    }
    let temp = scratch.join("tmp");
    fs::create_dir(&temp).unwrap();
    let harness = scratch.join("harness");
    for file in CODEX_0_167_0.files {
        let path = format!("{}/{}", CODEX_0_167_0.dir, file.path);
        put(&harness, &path, file.path.as_bytes(), 0o644);
    }
    let (bin, prefix) = (scratch.join("bin"), format!("pumpkin-frontier-cs/{task}/"));
    let mut lines = Vec::new();
    let mut images: Vec<String> = Vec::new();
    for &run in runs {
        let own = matches!(run, Run::Raced).then(|| own_image(&scratch, &images[0], &prefix));
        if let Run::Edited = run {
            put(&dir, "notes", b"two", 0o644);
        }
        put_stand_in(&bin, &run.stand_in(&dir, &prefix), &harness);
        let shown = run_build(&dir, context, &temp, &bin, &mut images);
        lines.push(format!("{run:?}: {shown}"));
        if let Some(own) = own {
            let inspected = docker().args(["image", "inspect", &own]).output().unwrap();
            let kept = if inspected.status.success() {
                "kept"
            } else {
                "dropped"
            };
            lines.push(format!("the image the moved tag named: {kept}"));
            let _ = docker().args(["image", "rm", &own]).output();
        }
    }
    let left = fs::read_dir(&temp)
        .unwrap()
        .map(|entry| entry.unwrap().file_name());
    let left: Vec<_> = left.collect();
    lines.push(format!("left in the runs' temporary directory: {left:?}"));
    let digest = images.first().map(|agent| {
        let (shown, digest) = agent_image(&scratch, agent, &prefix);
        lines.extend(shown);
        digest
    });
    for (letter, image) in (b'A'..).map(char::from).zip(&images) {
        lines.push(format!("{letter}: {}", untag(image, &prefix).join(" ")));
    }
    fs::remove_dir_all(&scratch).unwrap();
    let mut lines = lines
        .join("\n")
        .replace(&scratch.display().to_string(), "<scratch>");
    // The agent's tag is the digest of a recipe that names its base by the base's repository,
    // which holds this process's ID.
    if let Some(digest) = digest {
        lines = lines.replace(&digest, "<digest>");
    }
    expect.assert_eq(&lines.replace(&std::process::id().to_string(), "<pid>"));
}

/// What `check` shows of `agent`, the agent's image, whose tags start with `prefix`: its files,
/// and the tags of its base, which it removes; and the digest its tag holds.
fn agent_image(scratch: &Path, agent: &str, prefix: &str) -> (Vec<String>, String) {
    let [tag] = <[String; 1]>::try_from(tags(agent, &format!("{prefix}agent:"))).unwrap();
    let mut lines = vec![format!("A holds:\n{}", exported(scratch, &tag))];
    let listed = [
        "image",
        "ls",
        "--no-trunc",
        "--format",
        "{{.ID}}",
        "--filter",
    ];
    let reference = format!("reference={prefix}agent-base");
    let bases = docker().args(listed).arg(reference).output().unwrap();
    for base in String::from_utf8(bases.stdout).unwrap().lines() {
        lines.push(format!("A's base: {}", untag(base, prefix).join(" ")));
    }
    let (_, digest) = tag.rsplit_once(':').unwrap();
    (lines, digest.to_owned())
}

/// Runs `pumpkin-frontier-cs build` on the task in `dir` through `context`, with `temp` as its
/// temporary directory, and `bin` first on its `PATH`. Shows how it exits, the IDs it prints,
/// by the letter of each one's first appearance in `images`, and whether it shows progress, or,
/// if it fails, its message.
fn run_build(
    dir: &Path,
    context: &str,
    temp: &Path,
    bin: &Path,
    images: &mut Vec<String>,
) -> String {
    // With the whole environment, so that the variables `cargo xtask ci --docker` sets to
    // reach no engine reach the command too, which must keep them from `docker`.
    let program = env!("CARGO_BIN_EXE_pumpkin-frontier-cs");
    let mut command = stdx::process::command(program, Environment::Inherited);
    command.args(["build".as_ref(), dir.as_os_str()]);
    command
        .args(["--docker-context", context])
        .env("TMPDIR", temp);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = iter::once(bin.to_owned()).chain(std::env::split_paths(&path));
    command.env("PATH", std::env::join_paths(path).unwrap());
    let output = command.output().unwrap();
    let mut stdout = String::from_utf8(output.stdout).unwrap();
    if let Ok(Value::Object(printed)) = serde_json::from_str(&stdout) {
        for id in printed.values().filter_map(Value::as_str) {
            if !images.iter().any(|image| image == id) {
                images.push(id.to_owned());
            }
            let letter = images.iter().position(|image| image == id).unwrap();
            let letter = char::from(b'A' + u8::try_from(letter).unwrap());
            stdout = stdout.replace(id, &letter.to_string());
        }
    }
    // A build's progress differs from run to run, so only whether there is any is shown, and
    // of a run that fails, only the last line, the command's own message.
    let stderr = String::from_utf8(output.stderr).unwrap();
    let shown = match (output.status.success(), stderr.is_empty()) {
        (true, true) => "nothing on stderr",
        (true, false) => "progress on stderr",
        (false, _) => stderr.lines().next_back().unwrap_or_default(),
    };
    format!("{}: {stdout}{shown}", output.status)
}

/// Builds an image of the test's own, which nothing else tags, as it holds this process's ID;
/// moves to it the tag of `agent`, the task's agent image, whose tags start with `prefix`; and
/// returns its ID.
fn own_image(scratch: &Path, agent: &str, prefix: &str) -> String {
    let dir = scratch.join("own");
    put(&dir, "Dockerfile", b"FROM scratch\nCOPY id /\n", 0o644);
    put(&dir, "id", std::process::id().to_string().as_bytes(), 0o644);
    let args = ["build", "--quiet", "--provenance=false", "--"];
    let built = docker().args(args).arg(&dir).output().unwrap();
    assert!(built.status.success(), "{built:?}");
    let own = String::from_utf8(built.stdout).unwrap().trim().to_owned();
    let [tag] = <[String; 1]>::try_from(tags(agent, &format!("{prefix}agent:"))).unwrap();
    let tagged = docker().args(["tag", &own, &tag]).output().unwrap();
    assert!(tagged.status.success(), "{tagged:?}");
    own
}

/// Puts in `bin` a stand-in for `docker`: a script that does what `arms`, the arms of a shell
/// `case` on its arguments, say, and then runs the `docker` that `PATH` finds after `bin`, with
/// `harness` as the context of Codex's stage that unpacks its files, if it runs `docker build`.
fn put_stand_in(bin: &Path, arms: &str, harness: &Path) {
    let stage = format!("{}={}", CODEX_0_167_0.name, harness.display());
    let script = format!(
        r#"#!/bin/sh
case " $* " in
{arms}
esac
if [ "$2" = build ]; then
    context=$1
    shift 2
    set -- "$context" build --build-context '{stage}' "$@"
fi
PATH="${{PATH#*:}}" exec docker "$@"
"#
    );
    put(bin, "docker", script.as_bytes(), 0o755);
}

/// The files of the image tagged `tag`, as the engine exports them to a tarball in `scratch`:
/// each one's permission bits, owner and path.
fn exported(scratch: &Path, tag: &str) -> String {
    let tarball = scratch.join("exported.tar");
    let output = format!("--output=type=tar,dest={}", tarball.display());
    let mut build = docker()
        .args(["build", "--quiet", "--provenance=false", &output, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Dropping its standard input once written ends the Dockerfile.
    (build.stdin.take().unwrap())
        .write_all(format!("FROM {tag}\n").as_bytes())
        .unwrap();
    let exported = build.wait_with_output().unwrap();
    assert!(exported.status.success(), "{exported:?}");
    let list = ["--list", "--verbose", "--numeric-owner", "--file"];
    let tar = stdx::process::command("tar", Environment::Only(&["PATH"]));
    let listed = { tar }.args(list).arg(&tarball).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    let listed = String::from_utf8(listed.stdout).unwrap();
    let files = listed.lines().map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        format!("  {} {} {}", fields[0], fields[1], fields[fields.len() - 1])
    });
    files.collect::<Vec<_>>().join("\n")
}

/// The tags that `image` has which start with `prefix`, sorted.
fn tags(image: &str, prefix: &str) -> Vec<String> {
    let inspect = ["image", "inspect", "--format", "{{json .RepoTags}}", image];
    let output = docker().args(inspect).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let tags: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
    let mut tags: Vec<String> = tags
        .into_iter()
        .filter(|tag| tag.starts_with(prefix))
        .collect();
    tags.sort();
    tags
}

/// Removes the tags that `image` has which start with `prefix`, and so the image if no other tag
/// names it, and returns them, sorted.
fn untag(image: &str, prefix: &str) -> Vec<String> {
    let tags = tags(image, prefix);
    for tag in &tags {
        let removed = docker().args(["image", "rm", tag]).output();
        if !removed.is_ok_and(|removed| removed.status.success()) {
            eprintln!("the test's image tag {tag} can't be removed");
        }
    }
    tags
}

/// A task's images are built, each tagged by the digest of its recipe and the task's files, and
/// found by those tags when the command runs again, with nothing built; the agent's is its
/// base with Codex's files added, owned by root, with the modes Codex's pin gives them. A build
/// that finds no image under its tag, as in a race with another build of the same inputs, moves
/// the tag and keeps the image it named; one whose task's files change while it runs tags
/// nothing; and one whose recipe changes, once the task is read, into one the check refuses,
/// builds nothing. Each run removes the directories it writes a recipe and an image's ID to.
#[test]
#[ignore = "needs Docker: run cargo xtask ci --docker"]
fn build() {
    // Named by this process, so that tests that run at once tag images of their own.
    let task = format!("tiny-{}", std::process::id());
    let runs = [
        Run::Plain,
        Run::Plain,
        Run::Raced,
        Run::Edited,
        Run::Swapped,
    ];
    check(
        &task,
        &context(),
        &runs,
        &expect![[r#"
            Plain: exit status: 0: {
              "agent": "A",
              "verifier": "B"
            }
            progress on stderr
            Plain: exit status: 0: {
              "agent": "A",
              "verifier": "B"
            }
            nothing on stderr
            Raced: exit status: 0: {
              "agent": "A",
              "verifier": "B"
            }
            progress on stderr
            the image the moved tag named: kept
            Edited: exit status: 1: pumpkin-frontier-cs: pumpkin-frontier-cs/tiny-<pid>/agent-base: <scratch>/tiny-<pid> changed during the build, so the image isn't tagged: build again once nothing changes it
            Swapped: exit status: 1: pumpkin-frontier-cs: pumpkin-frontier-cs/tiny-<pid>/agent-base: <scratch>/tiny-<pid>/agent-base/Dockerfile:2: `http://localhost:1/notes` may be a URL, for `ADD` to fetch: copy files from the task's directory with `COPY`
            left in the runs' temporary directory: []
            A holds:
              dr-xr-xr-x 0/0 harness/
              dr-xr-xr-x 0/0 harness/codex-0.167.0/
              dr-xr-xr-x 0/0 harness/codex-0.167.0/bin/
              -r-xr-xr-x 0/0 harness/codex-0.167.0/bin/codex
              -r-xr-xr-x 0/0 harness/codex-0.167.0/bin/codex-code-mode-host
              -r--r--r-- 0/0 harness/codex-0.167.0/codex-package.json
              dr-xr-xr-x 0/0 harness/codex-0.167.0/codex-path/
              -r-xr-xr-x 0/0 harness/codex-0.167.0/codex-path/rg
              -rw-r--r-- 0/0 notes
            A's base: pumpkin-frontier-cs/tiny-<pid>/agent-base:181862722d663e8de1aa193668d40dc05e54e25432dba40cd166cffa43dd6592
            A: pumpkin-frontier-cs/tiny-<pid>/agent:<digest>
            B: pumpkin-frontier-cs/tiny-<pid>/verifier:83bfeac20ed3c170893778c2fdab88c1186849c2c413921f3177d64a8a9a55ce"#]],
    );
}

/// The task is read before the engine is reached, so a task it refuses needs no Docker.
#[test]
fn refusals() {
    check(
        "LWE",
        "pumpkin-tests-no-such-context",
        &[Run::Plain],
        &expect![[r#"
            Plain: exit status: 1: pumpkin-frontier-cs: <scratch>/LWE: the directory's name, the task's problem id: `LWE` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            left in the runs' temporary directory: []"#]],
    );
}
