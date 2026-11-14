//! `ec2-boot`, against [`STAND_IN`], a stand-in for the commands an instance runs it with,
//! `docker`, `nft` and `shutdown`, which records each call, so these tests need no instance.
//! Each runs `ec2-boot start` in a directory of its own, below which it keeps its files, as it
//! keeps them below `/` on an instance.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;
use std::{env, fs, iter};

use expect_test::{Expect, expect};
use schema::ec2_boot::Start;
use schema::sandbox::RegistrationGrant;
use stdx::process::Environment;
use tempfile::TempDir;

/// A time far past every test, in seconds since the Unix epoch.
const LATER: u64 = 4_102_444_800;

/// The stand-in for `docker`, `nft` and `shutdown`, which a boot runs through links to it, each
/// named as its command, in `bin`: it answers as each would on an instance whose Docker is 29.1.3
/// and supports every limit, and records each call in `calls`, its command and then its
/// arguments, a line each, the call ended by an empty line. `nft` keeps what it reads in
/// `nft-rules`; `docker cp` writes the path it copies from to the file it copies to; `docker run`
/// writes, to `env-file`, the mode and the text of the file that its `--env-file` names.
const STAND_IN: &str = r#"#!/bin/sh
dir=${0%/*}
name=${0##*/}
{ echo "$name"; printf '%s\n' "$@"; echo; } >> "$dir/calls"
case "$name $1" in
    "nft "*) cat > "$dir/nft-rules" ;;
    "shutdown "*|"docker pull") ;;
    "docker info") echo true true true true ;;
    "docker version") echo 29.1.3 ;;
    # The reference by digest that the image was pulled by.
    "docker image") for last; do :; done; echo "$last" ;;
    "docker logout") echo "Removing login credentials for $2" ;;
    "docker create") echo c0ffee ;;
    "docker cp") echo "copied from $2" > "$3" ;;
    "docker rm") echo "$2" ;;
    "docker run")
        for arg; do case $arg in --env-file=*) file=${arg#--env-file=} ;; esac; done
        { printf 'mode 0%s: ' "$(stat --format=%a -- "$file")"; cat -- "$file"; } > "$dir/env-file"
        echo 5f3c ;;
    "docker wait") echo 0 ;;
    "docker logs") echo 'a line on standard error' >&2; echo 'a line on standard output' ;;
    *) echo "the stand-in doesn't know \`$name $*\`" >&2; exit 1 ;;
esac
"#;

/// A directory of one boot's own: `bin`, which holds links to the stand-in, each named as a
/// command, and the files it writes, and `root`, which the boot runs in. Dropping it removes it.
struct Instance {
    dir: TempDir,
}

impl Instance {
    fn new(name: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("ec2-boot-{name}-"))
            .tempdir()
            .unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        // As the user data makes it, before `ec2-boot` runs.
        fs::create_dir_all(dir.path().join("root/run/pumpkin")).unwrap();
        let stand_in = dir.path().join("stand-in");
        fs::write(&stand_in, STAND_IN).unwrap();
        fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755)).unwrap();
        for command in ["docker", "nft", "shutdown"] {
            std::os::unix::fs::symlink(&stand_in, dir.path().join("bin").join(command)).unwrap();
        }
        Self { dir }
    }

    /// Runs `ec2-boot start` with `input` on its standard input, and returns its exit status and
    /// what it printed to the console.
    fn boot(&self, input: &str) -> Vec<String> {
        let path = env::var_os("PATH").unwrap_or_default();
        let path = iter::once(self.dir.path().join("bin")).chain(env::split_paths(&path));
        let boot = env!("CARGO_BIN_EXE_ec2-boot");
        let mut boot = stdx::process::command(boot, Environment::Inherited);
        boot.arg("start").current_dir(self.dir.path().join("root"));
        boot.env("PATH", env::join_paths(path).unwrap());
        let mut child = (boot.stdin(Stdio::piped()).stdout(Stdio::piped()))
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let console = String::from_utf8(output.stdout).unwrap();
        let mut lines = vec![format!("exit status: {:?}", output.status.code())];
        lines.extend(
            console
                .lines()
                .map(|line| format!("  {}", self.shown(line))),
        );
        lines
    }

    /// The calls the stand-in recorded, in the order made, each on a line.
    fn calls(&self) -> Vec<String> {
        let recorded = fs::read_to_string(self.dir.path().join("bin/calls")).unwrap_or_default();
        let calls = recorded.split_terminator("\n\n");
        calls
            .map(|call| self.shown(&call.lines().collect::<Vec<_>>().join(" ")))
            .collect()
    }

    /// The text of the file at `path`, below the directory, or that it is missing.
    fn file(&self, path: &str) -> String {
        let text = fs::read_to_string(self.dir.path().join(path));
        let text = text.map_or_else(|_| "missing".to_owned(), |text| self.shown(&text));
        format!("{path}: {text}")
    }

    /// `text`, with the directory shown as `<dir>`.
    fn shown(&self, text: &str) -> String {
        text.replace(self.dir.path().to_str().unwrap(), "<dir>")
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.dir.path());
    }
}

/// Runs `scenario` and checks the lines it reports against `expect`.
fn check(scenario: impl FnOnce() -> Vec<String>, expect: &Expect) {
    expect.assert_eq(&scenario().join("\n"));
}

/// A start of a role that makes ops and has a model route, whose deadlines are far off, as JSON.
fn start() -> Start {
    Start {
        hard_end_unix_seconds: LATER,
        pulls_by_unix_seconds: LATER - 3600,
        registry: "123456789012.dkr.ecr.us-west-1.amazonaws.com".to_owned(),
        role_image: "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa"
            .to_owned(),
        sandboxd_image: "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/sandboxd@sha256:bb"
            .to_owned(),
        confinement: vec!["--user=4242:4243".to_owned(), "--cap-drop=ALL".to_owned()],
        gateway: "172.31.0.10:7433".parse().unwrap(),
        grant: RegistrationGrant::from([0x5a; 16]),
        ops: true,
        model: true,
    }
}

// How a boot goes: what it says on the console, the commands it runs, in order, the rules it
// gives `nft`, the grant's file, as `docker run` found it, 0600, and gone once it returns, and
// the `sandboxd` it copied out of its image.
#[test]
fn boots() {
    check(
        || {
            let instance = Instance::new("boots");
            let mut lines = instance.boot(&serde_json::to_string(&start()).unwrap());
            lines.push("calls:".to_owned());
            lines.extend(instance.calls().iter().map(|call| format!("  {call}")));
            lines.push(instance.file("bin/nft-rules"));
            lines.push(instance.file("bin/env-file"));
            lines.push(instance.file("root/run/pumpkin/grant.env"));
            lines.push(instance.file("root/opt/pumpkin/sandboxd"));
            lines
        },
        &expect![[r#"
            exit status: Some(0)
              pumpkin-ec2: docker 29.1.3
              pumpkin-ec2: pulled 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa: 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa
              pumpkin-ec2: pulled 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/sandboxd@sha256:bb: 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/sandboxd@sha256:bb
              pumpkin-ec2: container 5f3c started
              pumpkin-ec2: container 5f3c exited with status 0
              pumpkin-ec2: the boot ended
              pumpkin-ec2: sandboxd: a line on standard output
              pumpkin-ec2: sandboxd: a line on standard error
            calls:
              nft --file -
              docker info --format {{.MemoryLimit}} {{.SwapLimit}} {{.CPUCfsQuota}} {{.PidsLimit}}
              docker version --format {{.Server.Version}}
              docker pull --quiet 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa
              docker image inspect --format {{index .RepoDigests 0}} 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa
              docker image inspect --format {{index .RepoDigests 0}} 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/sandboxd@sha256:bb
              docker logout 123456789012.dkr.ecr.us-west-1.amazonaws.com
              docker create --entrypoint=/ 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/sandboxd@sha256:bb
              docker cp c0ffee:/sandboxd opt/pumpkin/sandboxd
              docker rm c0ffee
              docker run --detach --name=pumpkin-sandbox --user=4242:4243 --cap-drop=ALL --cgroupns=private --network=bridge --dns=127.0.0.1 --dns-search=. --env-file=<dir>/root/run/pumpkin/grant.env --mount=type=bind,source=<dir>/root/opt/pumpkin/sandboxd,target=/.pumpkin/sandboxd,readonly --mount=type=bind,source=<dir>/root/opt/pumpkin/sandboxd,target=/.pumpkin/bin/sandboxctl,readonly --entrypoint=/.pumpkin/sandboxd -- 123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa 172.31.0.10:7433 --ops --model
              docker wait 5f3c
              docker logs --tail 20 5f3c
              shutdown --poweroff now
            bin/nft-rules: table inet pumpkin {
                chain forward {
                    type filter hook forward priority filter - 10; policy accept;
                    iifname "docker0" ip daddr 172.31.0.10 tcp dport 7433 accept
                    iifname "docker0" drop
                }
                chain input {
                    type filter hook input priority filter - 10; policy accept;
                    iifname "docker0" drop
                }
            }

            bin/env-file: mode 0600: SANDBOXD_GRANT=5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a

            root/run/pumpkin/grant.env: missing
            root/opt/pumpkin/sandboxd: copied from c0ffee:/sandboxd
        "#]],
    );
}
