//! The contributor tasks of the workspace, in the shape of
//! [matklad's `cargo xtask`](https://github.com/matklad/cargo-xtask/), run through the alias in
//! `.cargo/config.toml`.
//!
//! A check that every change must pass is a step of `ci`, as `tidy`'s are; the checks that reach
//! a real environment are `live`'s.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command line reports on its standard output and error"
)]

use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};
use std::time::Duration;

use repo::{Repo, STEP_TIMEOUT};
use stdx::Context as _;
use stdx::process::{self, Bound, Echo, Environment, Failure, Run, Stream, printed};

mod aws_certificates;
mod diff;
mod figure;
mod live;
mod op_stats;
mod page;
mod release;
mod repo;
mod settings;
mod star;
mod tidy;
mod tools;
mod wait;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// The target of the static binaries in an install.
const MUSL: &str = "x86_64-unknown-linux-musl";

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
            /// fails. They reach no Docker engine or cloud. They reach the network only to fetch
            /// crates and pinned tools on first use.
            cmd ci {}

            /// Writes the figures drawn from the workspace, which `ci` checks are up to date. The
            /// one figure, `docs/figures/crates.svg`, shows how the crates depend on one another.
            /// It is drawn from `cargo metadata`.
            cmd figures {}

            /// Rewrites the EC2 provider's table of AWS's identity certificates from AWS's page of
            /// them. The table is `providers/ec2/src/identity/certificates.rs`, which `ci` tests
            /// without reaching the network.
            cmd aws-certificates {}

            /// Runs the checks that reach a real environment, which `ci` leaves out. `docker`
            /// runs the ignored tests, which need a Docker engine, the static sandboxd or a server
            /// elsewhere. They use the context that `PUMPKIN_DOCKER_CONTEXT` names, or `default`.
            /// `ec2` and `modal` cost money. Each runs `pumpkin-admin check`, then `examples/smoke`
            /// on that provider through the controller that `~/.config/pumpkin/pumpkin.toml`
            /// names, with Pumpkin at `HEAD`, which `./pumpkin` builds below the target directory.
            /// Every probe of a sandbox's confinement must pass, and every sandbox must be deleted.
            /// If anything tagged with the run is left on EC2, EventBridge Scheduler or Modal, the
            /// check fails and runs `pumpkin cancel` to remove it.
            cmd live {
                /// `docker`, `ec2`, `modal` or `join`. `join` launches a new member's machine in
                /// the controller's VPC with the members' instance profile. It installs Pumpkin at
                /// `HEAD` there with the `[controller]` table and this machine's key, runs
                /// `pumpkin ls`, and ends the machine. It reaches the machine over `ssh` from the
                /// VPC. The machine costs under a cent, beside the controller's time if `pumpkin
                /// ls` starts it.
                required target: String
                /// With `modal`, the model of the GPU, of a class of `[controller.modal]`, that
                /// the smoke's GPU runner needs.
                optional --gpu-model model: String
                /// With `ec2` or `modal`, kills the run's job on the controller once the agent
                /// holds a lease of each grant. Checks that each sandbox ends by itself at the
                /// run's hard end, about 95 minutes after its start, and that `pumpkin reap` then
                /// removes what is left. Costs at most about $1 on EC2 and $4 on Modal.
                optional --lost
            }

            /// Counts incoming operations by request kind in a run's `log.jsonl`. Reports the
            /// bytes of matching lines, including their newlines, and the full log size.
            cmd op-stats {
                /// The run's record directory.
                required run_dir: PathBuf
                /// Only count requests with this caller name in the record, such as `job-17`.
                optional --caller name: String
            }

            /// Builds the static sandboxd that every sandbox runs and prints its path. Also builds
            /// the static ec2-boot beside it, which the EC2 provider's sandboxd image holds.
            cmd sandboxd {}

            /// Builds the files of an install, which `install::File` lists, and puts them in
            /// `<root>/bin`. Prints each file's path. The pinned tools require Linux on x86-64.
            cmd build {
                /// The directory whose `bin` they go in: this checkout's `.pumpkin` by default.
                optional --root dir: PathBuf
                /// The target for `pumpkin`, as in cargo's `--target`. The default is
                /// `x86_64-unknown-linux-musl`, built static with Zig as its C compiler.
                optional --target triple: String
            }

            /// Builds and checks the Linux x86-64 archive, launcher and pin from this checkout
            /// in `target/release-assets/<tag>/`.
            cmd release {
                /// The tag to put in the archive's name.
                required tag: String
                /// Publishes the existing files as the latest GitHub release for the tag.
                /// The default is to build them locally.
                optional --publish
            }

            /// Runs cargo-mutants on the working tree's changes. Each missed mutant, which
            /// `mutants.out` lists, is a finding to read, not a failure. Given paths that the
            /// changes leave as they are, it mutates their source whole. It then names the missed
            /// mutants that the tests of the merge base catch.
            cmd mutants {
                /// The paths whose changes alone are taken, if any are given.
                repeated paths: PathBuf
                /// The revision since whose merge base with `HEAD` the changes are taken,
                /// `origin/main` by default.
                optional --in-diff rev: String
            }

            /// Runs a package's tests with `UPDATE_EXPECT=1`, which rewrites each snapshot that
            /// differs. Then prints `git diff --stat` of the changes, to read before keeping them.
            cmd refill {
                /// The package whose tests run.
                required -p, --package package: String
                /// Only the tests whose names hold it.
                optional filter: String
            }

            /// Fetches each contributor tool that xtask runs, such as cargo-nextest, at its pinned
            /// version. The tools go into the cache that every checkout shares:
            /// `$XDG_CACHE_HOME/pumpkin/tools`, or `~/.cache/pumpkin/tools` when that is unset.
            /// It also installs the nightly rustfmt through rustup. It links the pinned buildifier
            /// at `bin/buildifier`, where the tests find it. Each command fetches the tools it runs
            /// on first use, so this one is for working offline later.
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
        Figures(Figures),
        AwsCertificates(AwsCertificates),
        Live(Live),
        OpStats(OpStats),
        Sandboxd(Sandboxd),
        Build(Build),
        Release(Release),
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
    pub struct Figures;

    #[derive(Debug)]
    pub struct AwsCertificates;

    #[derive(Debug)]
    pub struct Live {
        pub target: String,

        pub gpu_model: Option<String>,
        pub lost: bool,
    }

    #[derive(Debug)]
    pub struct OpStats {
        pub run_dir: PathBuf,

        pub caller: Option<String>,
    }

    #[derive(Debug)]
    pub struct Sandboxd;

    #[derive(Debug)]
    pub struct Build {
        pub root: Option<PathBuf>,
        pub target: Option<String>,
    }

    #[derive(Debug)]
    pub struct Release {
        pub tag: String,

        pub publish: bool,
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
    if let Some(failed) = tools::run_as_musl_cc() {
        return failed;
    }
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

/// xflags exits by itself on `--help`, with 0, and on a command line it can't parse, with 2.
// Each command is a function of its own, given its flags, as each of TigerBeetle's scripts is:
// https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/scripts.zig#L86-L93
fn try_main() -> Result<()> {
    let flags = flags::Xtask::from_env_or_exit();
    match flags.subcommand {
        flags::XtaskCmd::Fmt(flags::Fmt { check }) => {
            let mut fmt = fmt()?;
            if check {
                fmt.arg("--check");
            }
            ran(&mut fmt, step(STEP_TIMEOUT)).map(drop)
        }
        flags::XtaskCmd::Ci(flags::Ci) => ci(),
        flags::XtaskCmd::Figures(flags::Figures) => figure::write(root(), &metadata()?),
        flags::XtaskCmd::AwsCertificates(flags::AwsCertificates) => aws_certificates::write(),
        flags::XtaskCmd::Live(flags::Live {
            target,
            gpu_model,
            lost,
        }) => {
            // `join` runs on no provider of Pumpkin's.
            let provider = match target.as_str() {
                "docker" | "join" if lost => {
                    return Err("`--lost` is for `ec2` and `modal`: leave it out".into());
                }
                "docker" => return live::docker(),
                "join" => None,
                "ec2" => Some(settings::Provider::Ec2),
                "modal" => Some(settings::Provider::Modal),
                _ => {
                    return Err(format!(
                        "`{target}` is no live target: give `docker`, `ec2`, `modal` or `join`"
                    )
                    .into());
                }
            };
            if gpu_model.is_some() && provider != Some(settings::Provider::Modal) {
                return Err(
                    "`--gpu-model` is for `cargo xtask live modal` alone: leave it out".into(),
                );
            }
            match provider {
                Some(provider) => live::smoke(provider, gpu_model.as_deref(), lost),
                None => live::join(),
            }
        }
        flags::XtaskCmd::OpStats(flags::OpStats { run_dir, caller }) => {
            op_stats::report(&run_dir, caller.as_deref())
        }
        flags::XtaskCmd::Sandboxd(flags::Sandboxd) => {
            println!("{}", sandboxd()?.display());
            Ok(())
        }
        flags::XtaskCmd::Build(flags::Build { root, target }) => {
            build(root, target.as_deref().unwrap_or(MUSL))
        }
        flags::XtaskCmd::Release(flags::Release { tag, publish }) => release::run(&tag, publish),
        flags::XtaskCmd::Mutants(flags::Mutants { paths, in_diff }) => {
            mutants(&paths, in_diff.as_deref())
        }
        flags::XtaskCmd::Refill(flags::Refill { package, filter }) => {
            refill(&package, filter.as_deref())
        }
        flags::XtaskCmd::InstallTools(flags::InstallTools) => {
            tools::install()?;
            star::link().map(drop)
        }
    }
}

/// The bound of a step that builds or tests the workspace. A cold build on a busy machine takes
/// far longer than [`STEP_TIMEOUT`]. A step that outlasts the bound counts as hung, and xtask
/// kills it.
// GitHub Actions' default bound of a job:
// https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idtimeout-minutes
const BUILD_TIMEOUT: Duration = Duration::from_hours(6);

/// The bound of a run of cargo-mutants. cargo-mutants bounds the build and the tests of each
/// mutant itself. A run of a few crates whole takes many hours, so this bound only ends a
/// cargo-mutants that hangs.
// cargo-mutants' own timeouts: https://mutants.rs/timeouts.html
const MUTANTS_TIMEOUT: Duration = Duration::from_hours(7 * 24);

/// How xtask runs a step. It echoes the command and gives it this process's standard output and
/// error. It kills the command at `bound`.
fn step(bound: Duration) -> Run<'static> {
    Run::new(Bound::Within(bound), Echo::Line)
        .stdout(Stream::Inherited)
        .stderr(Stream::Inherited)
}

/// Runs `command` as `how` says, and fails with the message of its failure.
fn ran(command: &mut Command, how: Run<'_>) -> Result<Output> {
    Ok(process::run(command, how).map_err(|failure| failure.to_string())?)
}

/// What `command` printed, as text less its trailing newline. Its standard error goes to this
/// process's. It is killed at `bound`.
fn read(command: &mut Command, bound: Duration) -> Result<String> {
    let how = Run::new(Bound::Within(bound), Echo::Silent).stderr(Stream::Inherited);
    let output = ran(command, how)?;
    let shown = process::shown(command);
    Ok(printed(output).context(format_args!("reading what `{shown}` printed failed"))?)
}

/// A `cargo` with `args`, at the workspace's root. It gets this process's environment but the
/// variables that `cargo run` set for xtask's own package. The others stay, such as `CARGO` and
/// rustup's, which name the toolchain that it is to use.
fn cargo(args: &[&str]) -> Command {
    let mut cargo = process::command("cargo", Environment::Cargo);
    cargo.current_dir(root()).args(args);
    cargo
}

/// The command that runs `release`'s cargo subcommand, such as `nextest`, as [`cargo`] runs. It
/// runs the pinned release, not whichever one cargo finds, and fetches the release on first use.
/// `args` begin with the subcommand's name, as cargo passes it.
fn subcommand(release: tools::Release, args: &[&str]) -> Result<Command> {
    let mut command = process::command(tools::fetched(release)?, Environment::Cargo);
    command.current_dir(root()).args(args);
    Ok(command)
}

/// The pinned formatter. It installs the toolchain on first use, so that rustup never installs
/// the whole default profile implicitly. It ignores an inherited binary override.
fn fmt() -> Result<Command> {
    tools::rustfmt()?;
    let toolchain = format!("+{}", tools::RUSTFMT_TOOLCHAIN);
    let mut fmt = cargo(&[&toolchain, "fmt", "--all"]);
    fmt.env("RUSTUP_AUTO_INSTALL", "0").env_remove("RUSTFMT");
    Ok(fmt)
}

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

/// Runs the checks every change must pass, in order, and stops at the first that fails. The
/// tests leave out those ignored, which [`live::docker`] runs.
fn ci() -> Result<()> {
    ran(fmt()?.arg("--check"), step(STEP_TIMEOUT))?;
    let repo = Repo { root: root() };
    tidy::check(&repo)?;
    figure::check(root(), &metadata()?)?;
    let mut clippy = cargo(&["clippy", "--workspace", "--all-targets", "--locked"]);
    ran(
        clippy.args(["--", "--deny", "warnings"]),
        step(BUILD_TIMEOUT),
    )?;
    let mut doc = cargo(&["doc", "--no-deps", "--workspace", "--locked"]);
    ran(
        doc.env("RUSTDOCFLAGS", "--deny warnings"),
        step(BUILD_TIMEOUT),
    )?;
    page::check()?;
    let buildifier = star::link()?;
    let pumpkin = tests_pumpkin()?;
    star::place(&buildifier, &pumpkin)?;
    star::check(&repo, &pumpkin)?;
    let mut test = subcommand(
        tools::NEXTEST,
        &["nextest", "run", "--workspace", "--locked"],
    )?;
    let mut doctests = cargo(&["test", "--doc", "--workspace", "--locked"]);
    // ci checks snapshots, never rewrites them.
    test.env_remove("UPDATE_EXPECT");
    doctests.env_remove("UPDATE_EXPECT");
    tests(&mut test, &mut doctests, "cargo xtask ci")
}

/// What `cargo metadata` says of the workspace's own packages, as JSON.
fn metadata() -> Result<String> {
    let mut metadata = cargo(&["metadata", "--format-version", "1", "--no-deps", "--locked"]);
    read(&mut metadata, STEP_TIMEOUT)
}

/// The target directory, as `cargo metadata` reports it.
fn target_dir() -> Result<PathBuf> {
    let metadata: serde_json::Value = serde_json::from_str(&metadata()?)?;
    match metadata["target_directory"].as_str() {
        Some(dir) => Ok(dir.into()),
        None => Err("`cargo metadata` reported no target directory: expected its path".into()),
    }
}

/// Runs `test`, then `doctests`, each to its end. It fails if either fails, saying to run
/// `again`.
// One after the other, so that a terminal's Ctrl-C reaches both. The doctests take seconds.
fn tests(test: &mut Command, doctests: &mut Command, again: &str) -> Result<()> {
    let tested = process::run(test, step(BUILD_TIMEOUT).named("cargo nextest run"));
    let doctested = process::run(doctests, step(BUILD_TIMEOUT).named("cargo test --doc"));
    let mut failed = Vec::new();
    for ran in [tested, doctested] {
        match ran {
            Ok(_) => {}
            Err(Failure::Exited { name, output, .. }) => {
                failed.push(format!("`{name}` failed with {}", output.status));
            }
            Err(failure) => return Err(failure.to_string().into()),
        }
    }
    let they = match failed.len() {
        0 => return Ok(()),
        1 => "it",
        _ => "they",
    };
    let failed = failed.join(", and ");
    Err(format!("{failed}: fix what {they} printed above, and run `{again}` again").into())
}

/// Runs the tests of `package`, those whose names hold `filter` if it is given, with
/// `UPDATE_EXPECT=1`, which rewrites each snapshot that differs. Then it prints
/// `git diff --stat`, also if a test failed.
fn refill(package: &str, filter: Option<&str>) -> Result<()> {
    star::link()?;
    let test = ["nextest", "run", "--locked", "--package", package];
    let mut test = subcommand(tools::NEXTEST, &test)?;
    test.args(filter).env("UPDATE_EXPECT", "1");
    let tested = ran(&mut test, step(BUILD_TIMEOUT).named("cargo nextest run"));
    let repo = Repo { root: root() };
    // Git's pager would wait for a key, as git gets no `TERM`.
    ran(
        &mut repo.command(["--no-pager", "diff", "--stat"]),
        step(STEP_TIMEOUT),
    )?;
    tested.map(drop)
}

/// Runs cargo-mutants on the working tree's changes since `rev`, `origin/main` by default, below
/// `paths` alone if there are any. If the changes leave the source below `paths` as it is, it
/// mutates that source whole. Then it runs the mutants missed again on the merge base, to name
/// those that the changes left uncaught. Missed mutants and timeouts are findings, not failures.
fn mutants(paths: &[PathBuf], rev: Option<&str>) -> Result<()> {
    let buildifier = star::link()?;
    let mut again = "cargo xtask mutants".to_owned();
    for path in paths {
        again = format!("{again} {}", path.display());
    }
    if let Some(rev) = rev {
        again = format!("{again} --in-diff {rev}");
    }
    let repo = Repo { root: root() };
    let base = repo.git(["merge-base", rev.unwrap_or("origin/main"), "HEAD"])?;
    let diff = diff::since(&repo, &base, paths, &again)?;
    let scratch = tempfile::tempdir().context("making a scratch directory failed")?;
    let diff_path = scratch.path().join("changes.diff");
    fs::write(&diff_path, diff).context(format_args!("writing {} failed", diff_path.display()))?;
    // A glob with a slash matches the whole path in cargo-mutants, so a directory needs its `**`.
    let files = paths
        .iter()
        .flat_map(|path| [path.clone(), path.join("**")]);
    let files: Vec<_> = files
        .flat_map(|glob| ["--file".into(), glob.into_os_string()])
        .collect();
    let mut list = cargo_mutants()?;
    list.arg("--list-files");
    let whole = !paths.is_empty() && unchanged(&repo, list, &base, &files)?;
    let mut run = cargo_mutants()?;
    if whole {
        run.args(&files);
    } else {
        run.arg("--in-diff").arg(&diff_path);
    }
    let report = root().join("mutants.out");
    if !missed(&mut run, &again)? {
        return Ok(());
    }
    if !whole {
        println!(
            "xtask: the mutants cargo-mutants missed or timed out on are findings, listed in \
             `{}` with their diffs",
            report.display()
        );
        return Ok(());
    }
    // The source is as it is on `base`, so each mutant there has the name it has here, and
    // `--iterate` tests there only those that weren't caught or unviable here.
    let tree = tree(&repo, &base, scratch.path(), &buildifier)?;
    let mut run = cargo_mutants()?;
    run.args(["--iterate", "--dir"]).arg(&tree);
    missed(run.arg("--output").arg(root()).args(&files), &again)?;
    let caught = report.join("caught.txt");
    let brought =
        fs::read_to_string(&caught).context(format_args!("reading {} failed", caught.display()))?;
    let brought = if brought.is_empty() {
        "none\n"
    } else {
        &brought
    };
    println!(
        "xtask: the mutants cargo-mutants missed or timed out on are listed in `{}` with their \
         diffs; the changes left these uncaught, which the tests on {base} catch:\n{brought}",
        root().join("mutants.out.old").display()
    );
    Ok(())
}

/// A `cargo mutants` that runs the pinned cargo-nextest.
fn cargo_mutants() -> Result<Command> {
    // cargo-mutants runs `cargo nextest`, which runs the first cargo-nextest on `PATH`. The
    // pinned one goes first here. Cargo puts its own `bin`, where `cargo install` puts one,
    // before it only if `PATH` doesn't name that `bin`.
    let nextest = tools::fetched(tools::NEXTEST)?;
    let dir = nextest.parent().map(Path::to_path_buf);
    let path = env::var_os("PATH").unwrap_or_default();
    let path = env::join_paths(dir.into_iter().chain(env::split_paths(&path)))?;
    let mut run = subcommand(tools::MUTANTS, &["mutants"])?;
    run.env("PATH", path);
    Ok(run)
}

/// Writes the tree of `base` into `scratch` and returns its path. The tree holds the link to
/// `buildifier` that its tests find.
fn tree(repo: &Repo<'_>, base: &str, scratch: &Path, buildifier: &Path) -> Result<PathBuf> {
    let archive = scratch.join("base.tar");
    let archived = ["archive", "--output"].map(OsStr::new);
    repo.git(
        archived
            .into_iter()
            .chain([archive.as_os_str(), OsStr::new(base)]),
    )?;
    let tree = scratch.join("base");
    // The archive doesn't hold the link, which is ignored.
    let bin = tree.join("bin");
    fs::create_dir_all(&bin).context(format_args!("making {} failed", bin.display()))?;
    tools::unpack(&archive, &tree)?;
    let link = bin.join(install::File::Buildifier.name());
    let linked = std::os::unix::fs::symlink(buildifier, &link);
    linked.context(format_args!("making {} failed", link.display()))?;
    Ok(tree)
}

/// Whether the source files that `files`, cargo-mutants' `--file` arguments, select are the same
/// in the working tree as on `base`, and there are any. `list` is cargo-mutants' `--list-files`.
fn unchanged(repo: &Repo<'_>, mut list: Command, base: &str, files: &[OsString]) -> Result<bool> {
    let listed = read(list.args(files), STEP_TIMEOUT)?;
    let sources: Vec<&str> = listed.lines().collect();
    if sources.is_empty() {
        return Ok(false);
    }
    let mut diff = repo.command(["diff", "--quiet", base, "--"].into_iter().chain(sources));
    match process::run(
        &mut diff,
        Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent),
    ) {
        Ok(_) => Ok(true),
        // `--quiet` makes git exit with 1 where the files differ.
        Err(Failure::Exited { output, .. }) if output.status.code() == Some(1) => Ok(false),
        Err(failure) => Err(failure.to_string().into()),
    }
}

/// Runs `run`, a cargo-mutants command, and returns whether it missed a mutant or one timed out.
fn missed(run: &mut Command, again: &str) -> Result<bool> {
    // Relative, so that each copy of the tree cargo-mutants builds has its own: in a shared one,
    // cargo would take a mutant's artifacts as fresh for the unmutated tree.
    run.env("CARGO_TARGET_DIR", "target");
    match process::run(run, step(MUTANTS_TIMEOUT).named("cargo mutants")) {
        Ok(_) => Ok(false),
        // cargo-mutants exits with 2 when it missed a mutant and with 3 when one timed out.
        Err(Failure::Exited { output, .. }) if matches!(output.status.code(), Some(2 | 3)) => {
            Ok(true)
        }
        Err(failure @ Failure::Exited { .. }) => {
            Err(format!("{failure}, and run `{again}` again").into())
        }
        Err(failure) => Err(failure.to_string().into()),
    }
}

/// Builds the workspace's tests, as `cargo nextest run` then finds them, and returns the
/// `pumpkin` that they run.
fn tests_pumpkin() -> Result<PathBuf> {
    let artifacts = executables(cargo(&["test", "--workspace", "--locked", "--no-run"]))?;
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
fn sandboxd() -> Result<PathBuf> {
    release_binary("ec2-boot", MUSL)?;
    release_binary("sandboxd", MUSL)
}

/// Builds the release binary of `package`, which has its name, for `target`, and returns the path
/// that cargo reports for it.
fn release_binary(package: &str, target: &str) -> Result<PathBuf> {
    let mut build = cargo(&["build", "--release", "--locked", "--package", package]);
    build.args(["--target", target]);
    if package == "pumpkin" && target == MUSL {
        tools::musl_cc(&mut build)?;
    }
    let artifacts = executables(build)?;
    let mut named = artifacts
        .iter()
        .filter(|built| built["target"]["name"] == package);
    match named.find_map(|built| built["executable"].as_str()) {
        Some(path) => Ok(path.into()),
        None => Err(format!(
            "cargo reported no {package} executable: expected a compiler-artifact message for it"
        )
        .into()),
    }
}

/// Runs `build`, a cargo command that builds, and returns cargo's compiler-artifact message for
/// each executable it built or found fresh. So each path is right wherever cargo's configuration
/// puts the target directory.
fn executables(mut build: Command) -> Result<Vec<serde_json::Value>> {
    // One JSON message per line. Diagnostics are rendered on standard error instead.
    build.args(["--message-format", "json-render-diagnostics"]);
    let mut executables = Vec::new();
    for line in read(&mut build, BUILD_TIMEOUT)?.lines() {
        let message: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("cargo printed `{line}`, which isn't JSON: {error}"))?;
        if message["reason"] == "compiler-artifact" && message["executable"].is_string() {
            executables.push(message);
        }
    }
    Ok(executables)
}

/// Builds the files of an install, which [`install::File`] lists, with `pumpkin` for `target`.
/// Installs them in `<root>/bin`, made if need be, and replaces what is there. `<root>` is `root`
/// made absolute, or this checkout's `.pumpkin`. Prints each one's path. Without a `root`, says if
/// `<root>/bin` is not on `PATH`.
fn build(root: Option<PathBuf>, target: &str) -> Result<()> {
    let default_root = root.is_none();
    let root = root.unwrap_or_else(|| self::root().join(".pumpkin"));
    let bin = root.join("bin");
    let bin = std::path::absolute(&bin)
        .map_err(|error| format!("making {} absolute failed: {error}", bin.display()))?;
    // The Modal provider's helper and its lock, which `pumpkin` finds beside it, under the
    // names it looks for.
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers/modal/helper");
    let pumpkin = release_binary("pumpkin", target)?;
    let buildifier = star::buildifier()?;
    let sandboxd = sandboxd()?;
    fs::create_dir_all(&bin)
        .map_err(|error| format!("making {} failed: {error}", bin.display()))?;
    for file in install::File::ALL {
        let from = match file {
            install::File::Pumpkin => Some(pumpkin.clone()),
            install::File::PumpkinAdmin => None,
            install::File::Buildifier => Some(buildifier.clone()),
            install::File::Ec2Boot => Some(sandboxd.with_file_name(file.name())),
            install::File::Sandboxd => Some(sandboxd.clone()),
            install::File::ModalHelper | install::File::ModalLock => Some(helper.join(file.name())),
        };
        let mode = if file.is_executable() { 0o755 } else { 0o644 };
        let placed = match &from {
            Some(from) => Placed::Copy(from, mode),
            // The table lists `pumpkin` first, so the link has its target.
            None => Placed::Link(install::File::Pumpkin.name()),
        };
        let to = install_file(&bin, file.name(), placed)?;
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

/// What [`install_file`] puts in an install.
#[derive(Clone, Copy)]
enum Placed<'a> {
    /// A copy of a file, with these permission bits.
    Copy(&'a Path, u32),
    /// A hard link to the file of this name beside it. The install and its archive then hold the
    /// bytes once.
    Link(&'a str),
}

/// Puts `placed` at `name` in `bin`, replacing what is there, and returns its path.
fn install_file(bin: &Path, name: &str, placed: Placed<'_>) -> Result<PathBuf> {
    let to = bin.join(name);
    // Linux refuses a write to a running executable. Rename the finished file into place.
    let staged = bin.join(format!(".{name}.xtask-build.{}", std::process::id()));
    let installed = match placed {
        Placed::Copy(from, mode) => fs::copy(from, &staged)
            .and_then(|_| fs::set_permissions(&staged, Permissions::from_mode(mode))),
        Placed::Link(target) => fs::hard_link(bin.join(target), &staged),
    };
    if let Err(error) = installed.and_then(|()| fs::rename(&staged, &to)) {
        let _ = fs::remove_file(&staged);
        return Err(format!("installing {} failed: {error}", to.display()).into());
    }
    Ok(to)
}
