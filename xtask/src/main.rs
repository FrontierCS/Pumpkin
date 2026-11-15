//! See <https://github.com/matklad/cargo-xtask/>.
//!
//! This is integrated as a `cargo` subcommand by using an alias in `.cargo/config.toml`.
//!
//! A check that every change must pass is a step of `ci`, as `tidy`'s are; the checks that reach
//! a real environment are `live`'s.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command line reports on its standard output and error"
)]

use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::{env, iter};

use xshell::{Cmd, Shell};

mod diff;
mod live;
mod settings;
mod star;
mod tidy;
mod tools;
mod wait;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The target the static `sandboxd` and `ec2-boot` are built for.
const MUSL: &str = "x86_64-unknown-linux-musl";

const RUSTFMT_TOOLCHAIN: &str = "nightly-2026-11-07";

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

    use std::path::PathBuf;

    xflags::xflags! {
        src "./src/main.rs"

        /// Contributor tasks.
        cmd xtask {
            /// Formats the workspace with the pinned nightly rustfmt.
            cmd fmt {
                /// Checks formatting without changing files.
                optional --check
            }

            /// Runs the checks every change must pass, in order, and stops at the first that
            /// fails. They reach no Docker engine, network or cloud.
            cmd ci {}

            /// Runs the checks that reach a real environment, which `ci` leaves out: `docker`, the
            /// tests that need a Docker engine, the static sandboxd or a server elsewhere, ignored
            /// otherwise, through the Docker context `PUMPKIN_DOCKER_CONTEXT` names, or
            /// `default`; `ec2` and `modal`, which cost money, a run of `examples/smoke` on that
            /// provider through the controller that `~/.config/pumpkin/pumpkin.toml` names, with
            /// Pumpkin at `HEAD`, built by `./pumpkin` in a task repository below the target
            /// directory: every probe of a sandbox's confinement must pass, every sandbox be
            /// deleted, and nothing tagged with the run be left on EC2, EventBridge Scheduler or
            /// Modal, which `pumpkin cancel` then removes.
            cmd live {
                /// `docker`, `ec2` or `modal`.
                required target: String
                /// With `modal`, the model of the GPU, of a class of `[controller.modal]`, that
                /// the smoke's GPU runner needs.
                optional --gpu-model model: String
                /// With `ec2` or `modal`, loses the run once the agent holds a lease of each
                /// grant, by killing its job on the controller, and checks that each sandbox ends
                /// by itself by the run's hard end, about 95 minutes after its start, and that
                /// `pumpkin reap` then removes what is left: at most about $1 on EC2, $4 on Modal.
                optional --lost
            }

            /// Builds the static sandboxd that every sandbox runs, and beside it the static
            /// ec2-boot that the EC2 provider's image of it holds too, and prints sandboxd's path.
            cmd sandboxd {}

            /// Builds `pumpkin`, the static sandboxd and ec2-boot, and installs them side by side
            /// in `<root>/bin`, with the Modal provider's helper and its lock, where `pumpkin`
            /// finds that sandboxd by default, and that helper, and with the pinned buildifier,
            /// which `pumpkin fmt` and `pumpkin check` run; prints each path. It runs on Linux
            /// on x86-64 alone, the one platform buildifier is pinned for.
            cmd install {
                /// The directory whose `bin` they go in: this checkout's `.pumpkin` by default.
                optional --root dir: PathBuf
            }

            /// Runs cargo-mutants on the working tree's changes: each mutant it reports missed,
            /// listed in `mutants.out`, is a finding to read, not a failure.
            cmd mutants {
                /// The paths whose changes alone are taken, if any are given.
                repeated paths: PathBuf
                /// The revision since whose merge base with `HEAD` the changes are taken,
                /// `origin/main` by default.
                optional --in-diff rev: String
            }

            /// Runs a package's tests with `UPDATE_EXPECT=1`, which rewrites each snapshot that
            /// differs in this checkout, then prints `git diff --stat` of what changed, to read
            /// before keeping it.
            cmd refill {
                /// The package whose tests run.
                required -p, --package package: String
                /// Only the tests whose names hold it.
                optional filter: String
            }

            /// Installs each contributor tool that xtask runs, such as cargo-nextest, at its
            /// pinned version.
            cmd install-tools {}
        }
    }
    // generated start
    // The following code is generated by `xflags` macro.
    // Run `env UPDATE_XFLAGS=1 cargo build` to regenerate.
    #[derive(Debug)]
    pub struct Xtask {
        pub subcommand: XtaskCmd,
    }

    #[derive(Debug)]
    pub enum XtaskCmd {
        Fmt(Fmt),
        Ci(Ci),
        Live(Live),
        Sandboxd(Sandboxd),
        Install(Install),
        Mutants(Mutants),
        Refill(Refill),
        InstallTools(InstallTools),
    }

    #[derive(Debug)]
    pub struct Fmt {
        pub check: bool,
    }

    #[derive(Debug)]
    pub struct Ci;

    #[derive(Debug)]
    pub struct Live {
        pub target: String,

        pub gpu_model: Option<String>,

        pub lost: bool,
    }

    #[derive(Debug)]
    pub struct Sandboxd;

    #[derive(Debug)]
    pub struct Install {
        pub root: Option<PathBuf>,
    }

    #[derive(Debug)]
    pub struct Mutants {
        pub paths: Vec<PathBuf>,

        pub in_diff: Option<String>,
    }

    #[derive(Debug)]
    pub struct Refill {
        pub filter: Option<String>,

        pub package: String,
    }

    #[derive(Debug)]
    pub struct InstallTools;

    impl Xtask {
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

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// xflags exits by itself on `--help`, with 0, and on a command line it can't parse, with 2.
fn try_main() -> Result<()> {
    let flags = flags::Xtask::from_env_or_exit();
    let cargo = Cargo::new()?;
    match flags.subcommand {
        flags::XtaskCmd::Fmt(flags::Fmt { check }) => {
            let fmt = cargo.fmt();
            let fmt = if check { fmt.arg("--check") } else { fmt };
            fmt.run()?;
            Ok(())
        }
        flags::XtaskCmd::Ci(flags::Ci) => ci(&cargo),
        flags::XtaskCmd::Live(flags::Live {
            target,
            gpu_model,
            lost,
        }) => {
            let provider = match target.as_str() {
                "docker" if lost => {
                    return Err("`--lost` is for `ec2` and `modal`: leave it out".into());
                }
                "docker" => return live::docker(&cargo),
                "ec2" => settings::Provider::Ec2,
                "modal" => settings::Provider::Modal,
                _ => {
                    return Err(format!(
                        "`{target}` is no live target: give `docker`, `ec2` or `modal`"
                    )
                    .into());
                }
            };
            if gpu_model.is_some() && provider != settings::Provider::Modal {
                return Err(
                    "`--gpu-model` is for `cargo xtask live modal` alone: leave it out".into(),
                );
            }
            live::smoke(&cargo, provider, gpu_model.as_deref(), lost)
        }
        flags::XtaskCmd::Sandboxd(flags::Sandboxd) => {
            println!("{}", sandboxd(&cargo)?.display());
            Ok(())
        }
        flags::XtaskCmd::Install(flags::Install { root }) => install(&cargo, root),
        flags::XtaskCmd::Mutants(flags::Mutants { paths, in_diff }) => {
            mutants(&cargo, &paths, in_diff.as_deref())
        }
        flags::XtaskCmd::Refill(flags::Refill { package, filter }) => {
            refill(&cargo, &package, filter.as_deref())
        }
        flags::XtaskCmd::InstallTools(flags::InstallTools) => tools::install(&cargo),
    }
}

/// Builds the cargo commands xtask runs, at the workspace's root, without the variables that
/// `cargo run` sets for xtask's own package ([`stdx::process::cargo_package_vars`]). The others
/// stay, such as `CARGO` and rustup's, which name the toolchain a child cargo is to use. xshell's
/// `Shell` can set a variable for all its commands but not remove one, so each command removes
/// them.
struct Cargo {
    sh: Shell,
    package_vars: Vec<OsString>,
}

impl Cargo {
    fn new() -> Result<Self> {
        let sh = Shell::new()?;
        sh.change_dir(root());
        let package_vars = stdx::process::cargo_package_vars();
        Ok(Self { sh, package_vars })
    }

    fn cmd(&self, args: &[&str]) -> Cmd<'_> {
        let cmd = self.sh.cmd("cargo").args(args);
        self.package_vars.iter().fold(cmd, Cmd::env_remove)
    }

    /// The pinned formatter, without implicit installation or an inherited binary override.
    fn fmt(&self) -> Cmd<'_> {
        let toolchain = format!("+{RUSTFMT_TOOLCHAIN}");
        self.cmd(&[&toolchain, "fmt", "--all"])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env_remove("RUSTFMT")
    }
}

/// The workspace's root, which holds `xtask/`.
fn root() -> &'static Path {
    let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = xtask.parent() else {
        unreachable!(
            "xtask's directory {} has no parent, though it is the workspace's `xtask/`, below \
             its root, and only a root or an empty path has none",
            xtask.display()
        )
    };
    root
}

/// `tests`, a command that runs tests, with the environment they need: `PATH` with `xtask/poison`
/// first, whose `aws` fails, so that none reaches AWS, then xtask's own; and
/// `PUMPKIN_BUILDIFIER`, the pinned formatter ([`star::buildifier`]), which pumpkin's tests put
/// beside each `pumpkin` they run, where `pumpkin fmt` and `pumpkin check` find it.
fn tested<'a>(cargo: &Cargo, tests: Cmd<'a>) -> Result<Cmd<'a>> {
    let path = env::var_os("PATH").unwrap_or_default();
    let poisoned = iter::once(root().join("xtask/poison")).chain(env::split_paths(&path));
    let tests = tests.env("PATH", env::join_paths(poisoned)?);
    Ok(tests.env("PUMPKIN_BUILDIFIER", star::buildifier(cargo)?))
}

/// Runs the checks every change must pass, in order, and stops at the first that fails. The
/// tests ([`tested`]) leave out those ignored, which [`live::docker`] runs.
fn ci(cargo: &Cargo) -> Result<()> {
    tools::require(cargo, tools::NEXTEST)?;
    cargo.fmt().arg("--check").run()?;
    tidy::check(&cargo.sh)?;
    let clippy = cargo.cmd(&["clippy", "--workspace", "--all-targets", "--locked"]);
    clippy.args(["--", "--deny", "warnings"]).run()?;
    let doc = cargo.cmd(&["doc", "--no-deps", "--workspace", "--locked"]);
    doc.env("RUSTDOCFLAGS", "--deny warnings").run()?;
    let buildifier = star::buildifier(cargo)?;
    let pumpkin = tests_pumpkin(cargo)?;
    star::place(&buildifier, &pumpkin)?;
    star::check(&cargo.sh, &pumpkin)?;
    let test = cargo.cmd(&["nextest", "run", "--workspace", "--locked"]);
    let test = tested(cargo, test)?;
    let doctests = cargo.cmd(&["test", "--doc", "--workspace", "--locked"]);
    let doctests = tested(cargo, doctests)?;
    // ci checks snapshots, never rewrites them.
    tests(
        test.env_remove("UPDATE_EXPECT"),
        doctests.env_remove("UPDATE_EXPECT"),
        "cargo xtask ci",
    )
}

/// Runs `test` and `doctests` at once, and prints what `doctests` printed, if it failed, once
/// `test` has ended. Fails if either fails, saying to run `again`.
fn tests(test: Cmd<'_>, doctests: Cmd<'_>, again: &str) -> Result<()> {
    let (test_line, doctests_line) = (test.to_string(), doctests.to_string());
    let doctests = Command::from(doctests)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let doctests =
        doctests.map_err(|error| format!("starting `{doctests_line}` failed: {error}"))?;
    let tested = Command::from(test).status();
    let tested = tested.map_err(|error| format!("running `{test_line}` failed: {error}"))?;
    let doctested = doctests.wait_with_output();
    let doctested =
        doctested.map_err(|error| format!("waiting for `{doctests_line}` failed: {error}"))?;
    if doctested.status.success() {
        println!("xtask: `{doctests_line}` passed");
    } else {
        eprint!("{}", String::from_utf8_lossy(&doctested.stderr));
        print!("{}", String::from_utf8_lossy(&doctested.stdout));
    }
    let ended = [(test_line, tested), (doctests_line, doctested.status)];
    let failed: Vec<_> = (ended.iter().filter(|(_, status)| !status.success()))
        .map(|(line, status)| format!("`{line}` ended with {status}"))
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    let failed = failed.join(", and ");
    Err(format!("{failed}: fix what it printed above, and run `{again}` again").into())
}

/// Runs the tests of `package`, those whose names hold `filter` if it is given, with the
/// environment `ci` gives them ([`tested`]) and `UPDATE_EXPECT=1`, which rewrites each snapshot
/// that differs, below `CARGO_WORKSPACE_DIR`, this checkout's root, whatever the caller's
/// environment names; then prints `git diff --stat`, also if a test failed.
fn refill(cargo: &Cargo, package: &str, filter: Option<&str>) -> Result<()> {
    tools::require(cargo, tools::NEXTEST)?;
    let test = cargo.cmd(&["nextest", "run", "--locked", "--package", package]);
    let test = tested(cargo, test.args(filter))?.env("UPDATE_EXPECT", "1");
    // expect-test takes the variable as a directory that ends in `/`.
    let ran = test
        .env("CARGO_WORKSPACE_DIR", format!("{}/", root().display()))
        .run();
    cargo.sh.cmd("git").args(["diff", "--stat"]).run()?;
    Ok(ran?)
}

/// Runs cargo-mutants on the working tree's changes since `rev`, `origin/main` by default, below
/// `paths` alone if there are any. Missed mutants and timeouts are findings, not failures.
fn mutants(cargo: &Cargo, paths: &[PathBuf], rev: Option<&str>) -> Result<()> {
    tools::require(cargo, tools::MUTANTS)?;
    tools::require(cargo, tools::NEXTEST)?;
    let mut again = "cargo xtask mutants".to_owned();
    for path in paths {
        again = format!("{again} {}", path.display());
    }
    if let Some(rev) = rev {
        again = format!("{again} --in-diff {rev}");
    }
    let diff = diff::since(&cargo.sh, rev.unwrap_or("origin/main"), paths, &again)?;
    let scratch = cargo.sh.create_temp_dir()?;
    let diff_path = scratch.path().join("changes.diff");
    cargo.sh.write_file(&diff_path, diff)?;
    let run = tested(cargo, cargo.cmd(&["mutants", "--in-diff"]).arg(&diff_path))?;
    // Relative, so that each copy of the tree cargo-mutants builds has its own: in a shared one,
    // cargo would take a mutant's artifacts as fresh for the unmutated tree.
    let run = run.env("CARGO_TARGET_DIR", "target");
    let line = run.to_string();
    let status = Command::from(run).status();
    let status = status.map_err(|error| format!("running `{line}` failed: {error}"))?;
    // cargo-mutants exits with 2 when the tests missed a mutant, and 3 when one timed out.
    match status.code() {
        Some(0) => Ok(()),
        Some(2 | 3) => {
            let report = root().join("mutants.out");
            println!(
                "xtask: the mutants cargo-mutants missed or timed out on are findings, listed in \
                 `{}` with their diffs",
                report.display()
            );
            Ok(())
        }
        _ => Err(format!(
            "`{line}` ended with {status}: fix what it printed above, and run `{again}` again"
        )
        .into()),
    }
}

/// Builds the workspace's tests, as `cargo nextest run` then finds them, and returns the
/// `pumpkin` that they run.
fn tests_pumpkin(cargo: &Cargo) -> Result<PathBuf> {
    let build = cargo.cmd(&["test", "--workspace", "--locked", "--no-run"]);
    let artifacts = executables(build)?;
    let mut named = artifacts.iter().filter(|built| {
        built["target"]["name"] == "pumpkin" && built["target"]["kind"][0] == "bin"
    });
    match named.find_map(|built| built["executable"].as_str()) {
        Some(pumpkin) => Ok(pumpkin.into()),
        None => Err("cargo reported no pumpkin executable for the tests: expected one".into()),
    }
}

/// Builds the static `sandboxd` that every sandbox runs, and the static `ec2-boot`, which cargo
/// puts beside it, where the EC2 provider finds it, and returns `sandboxd`'s path.
fn sandboxd(cargo: &Cargo) -> Result<PathBuf> {
    release(cargo, "ec2-boot", &["--target", MUSL])?;
    release(cargo, "sandboxd", &["--target", MUSL])
}

/// Builds the release binary of `package`, which has its name, with the cargo arguments `args`,
/// and returns the path cargo reports for it, so that the path is right wherever cargo's
/// configuration puts the target directory.
fn release(cargo: &Cargo, package: &str, args: &[&str]) -> Result<PathBuf> {
    let build = cargo.cmd(&["build", "--release", "--locked", "--package", package]);
    let build = build.args(args);
    let build = build.args(["--message-format", "json-render-diagnostics"]);
    // One JSON message per line; diagnostics are rendered on standard error instead.
    for line in build.read()?.lines() {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("cargo printed `{line}`, which isn't JSON: {error}"))?;
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == package
            && let Some(path) = message["executable"].as_str()
        {
            return Ok(path.into());
        }
    }
    Err(format!(
        "cargo reported no {package} executable: expected a compiler-artifact message for it"
    )
    .into())
}

/// Runs `build`, a cargo command that builds, and returns cargo's compiler-artifact message for
/// each executable it built or found fresh, so that each path is right wherever cargo's
/// configuration puts the target directory.
fn executables(build: Cmd<'_>) -> Result<Vec<serde_json::Value>> {
    let build = build.args(["--message-format", "json-render-diagnostics"]);
    let mut executables = Vec::new();
    // One JSON message per line; diagnostics are rendered on standard error instead.
    for line in build.read()?.lines() {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("cargo printed `{line}`, which isn't JSON: {error}"))?;
        if message["reason"] == "compiler-artifact" && message["executable"].is_string() {
            executables.push(message);
        }
    }
    Ok(executables)
}

/// Builds `pumpkin`, the static `sandboxd` and `ec2-boot`, and installs them, with the Modal
/// provider's helper and its lock, and the pinned buildifier ([`star::buildifier`]), in
/// `<root>/bin`, made if need be, replacing what is there: `root` made absolute, or this
/// checkout's `.pumpkin`. Prints each one's path, and, without a `root`, says if `<root>/bin` is
/// not on `PATH`.
fn install(cargo: &Cargo, root: Option<PathBuf>) -> Result<()> {
    let default_root = root.is_none();
    let root = root.unwrap_or_else(|| self::root().join(".pumpkin"));
    let bin = root.join("bin");
    let bin = std::path::absolute(&bin)
        .map_err(|error| format!("making {} absolute failed: {error}", bin.display()))?;
    // The Modal provider's helper and its lock, which `pumpkin` finds beside it, under the
    // names it looks for.
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/modal/helper");
    let mut built = vec![
        ("pumpkin", release(cargo, "pumpkin", &[])?, 0o755),
        ("buildifier", star::buildifier(cargo)?, 0o755),
    ];
    let sandboxd = sandboxd(cargo)?;
    built.extend([
        ("ec2-boot", sandboxd.with_file_name("ec2-boot"), 0o755),
        ("sandboxd", sandboxd, 0o755),
        ("pumpkin-modal.py", helper.join("pumpkin-modal.py"), 0o644),
        (
            "pumpkin-modal.py.lock",
            helper.join("pumpkin-modal.py.lock"),
            0o644,
        ),
    ]);
    fs::create_dir_all(&bin)
        .map_err(|error| format!("making {} failed: {error}", bin.display()))?;
    for (name, from, mode) in built {
        let to = bin.join(name);
        // Copied beside it, then renamed over it: a copy over a `pumpkin` that runs would fail,
        // as Linux refuses to write a file that a process executes.
        let copy = bin.join(format!(".{name}.xtask-install"));
        // Each sandbox's processes run as its role's user, who must execute `sandboxd`.
        let installed = (fs::copy(from, &copy))
            .and_then(|_| fs::set_permissions(&copy, Permissions::from_mode(mode)))
            .and_then(|()| fs::rename(&copy, &to));
        installed.map_err(|error| format!("installing {} failed: {error}", to.display()))?;
        println!("{}", to.display());
    }
    let path = env::var_os("PATH").unwrap_or_default();
    if default_root && !env::split_paths(&path).any(|dir| dir == bin) {
        eprintln!(
            "xtask: {} is not on PATH: add it there to run `pumpkin` by its name",
            bin.display()
        );
    }
    Ok(())
}
