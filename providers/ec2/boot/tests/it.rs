//! `ec2-boot`, which refuses user data it can't read before it runs a command. What it runs on
//! an instance, `docker`, `nft`, `busctl` and the metadata's `curl`, is left to `cargo xtask live
//! ec2`, as each EC2 sandbox boots by it. The test runs `ec2-boot start` in a directory of its
//! own, below which it keeps its files, as it keeps them below `/` on an instance.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]
#![expect(clippy::disallowed_methods, reason = "nextest bounds each test")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Stdio;

use expect_test::{Expect, expect};
use schema::ec2_boot::{Start, USER_DATA_FILE, user_data};
use stdx::process::Environment;
use test_support::TestDir;

/// A time far past every test, in seconds since the Unix epoch.
const LATER: u64 = 4_102_444_800;

/// A directory of one boot's own: `bin`, empty, the boot's whole `PATH`, so that no command it
/// runs is found, and `root`, which the boot runs in.
struct Instance {
    dir: TestDir,
}

impl Instance {
    fn new() -> Self {
        let dir = TestDir::new();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        // As cloud-init makes it, before `ec2-boot` runs.
        fs::create_dir_all(dir.path().join("root/var/lib/cloud/instance")).unwrap();
        fs::create_dir_all(dir.path().join("root/run")).unwrap();
        Self { dir }
    }

    /// Runs `ec2-boot start` with `user_data` as cloud-init keeps it, and returns its exit status
    /// and what it printed to the console.
    fn boot(&self, user_data: &str) -> Vec<String> {
        fs::write(self.root(USER_DATA_FILE), user_data).unwrap();
        let boot = env!("CARGO_BIN_EXE_ec2-boot");
        let mut boot = stdx::process::command(boot, Environment::Inherited);
        boot.arg("start").current_dir(self.dir.path().join("root"));
        boot.env("PATH", self.dir.path().join("bin"));
        let output = (boot.stdin(Stdio::null()).stdout(Stdio::piped()))
            .output()
            .unwrap();
        let console = String::from_utf8(output.stdout).unwrap();
        let mut lines = vec![format!("exit status: {:?}", output.status.code())];
        lines.extend(
            console
                .lines()
                .map(|line| format!("  {}", self.shown(line))),
        );
        lines
    }

    /// The text of the file at `path`, below the directory, or that it is missing.
    fn file(&self, path: &str) -> String {
        let text = fs::read_to_string(self.dir.path().join(path));
        let text = text.map_or_else(|_| "missing".to_owned(), |text| self.shown(&text));
        format!("{path}: {text}")
    }

    /// Where `path`, absolute on an instance, is below the directory the boot runs in.
    fn root(&self, path: &str) -> std::path::PathBuf {
        self.dir
            .path()
            .join("root")
            .join(path.trim_start_matches('/'))
    }

    fn shown(&self, text: &str) -> String {
        text.replace(self.dir.path().to_str().unwrap(), "<dir>")
    }
}

/// Boots with `user_data`, and compares the console and the files left with `expect`.
/// Whatever the user data holds, the boot makes the attestation's directory root's alone.
fn check(user_data: &str, expect: &Expect) {
    let instance = Instance::new();
    let mut lines = instance.boot(user_data);
    let attestation_dir = fs::metadata(instance.root("/run/pumpkin")).unwrap();
    assert_eq!(attestation_dir.permissions().mode() & 0o777, 0o700);
    lines.push(instance.file("root/run/pumpkin/attestation.env"));
    lines.push(instance.file("root/opt/pumpkin/sandboxd"));
    expect.assert_eq(&lines.join("\n"));
}

/// The cloud-config of the user data, which the boot doesn't read.
const CLOUD_CONFIG: &str = "#cloud-config\n{}\n";

/// A start of a role that makes ops and has a model route, whose deadlines are far off, as JSON.
fn start() -> Start {
    Start {
        hard_end_unix_seconds: LATER,
        pulls_by_unix_seconds: LATER - 3600,
        role_image: "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/roles@sha256:aa"
            .to_owned(),
        sandboxd_image: "123456789012.dkr.ecr.us-west-1.amazonaws.com/pumpkin/sandboxd@sha256:bb"
            .to_owned(),
        confinement: vec!["--user=4242:4243".to_owned(), "--cap-drop=ALL".to_owned()],
        gateway: "172.31.0.10:7433".parse().unwrap(),
        ops: true,
        model: true,
    }
}

#[test]
fn refuses_a_start_it_cant_read() {
    // A start that doesn't parse, which fails the boot before it runs a command, with its directory
    // root's alone.
    let user_data = user_data(CLOUD_CONFIG, &start()).unwrap();
    let unread = user_data.replacen("{\"hard_end", "{\"registry\":\"\",\"hard_end", 1);
    check(
        &unread,
        &expect![[r#"
            exit status: Some(1)
              pumpkin-ec2: the boot failed: the user data in var/lib/cloud/instance/user-data.txt can't be read: its start can't be read, at line 1 of the start, column 11: expected a start's JSON, as the provider writes it
            root/run/pumpkin/attestation.env: missing
            root/opt/pumpkin/sandboxd: missing"#]],
    );
}
