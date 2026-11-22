//! `cargo xtask live examples`: a run of each shipped example through the controller, with real
//! models, from a task repository made of `examples/` at `HEAD`, as the examples' READMEs say to
//! make one, each checked as its README says. A release is made only of a commit on which every
//! run passed, which the commit status [`gate::CONTEXT`] records on GitHub.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use stdx::process::{self, Bound, Echo, Failure, Run};

use super::{
    checked_deployment, ended, head_commit, install, left, made, probes, pumpkin, reached, settings,
};
use crate::repo::{Repo, STEP_TIMEOUT};
use crate::settings::Settings;
use crate::{Result, gate, program, ran, read, resume, root, step, target_dir, tools, wait};

/// How many runs go at once: the native benchmark's, which runs longest, and two others.
const AT_ONCE: usize = 3;

/// How a run is checked once it has finished with every sandbox deleted.
enum Check {
    /// By the integers that its final call returned under these names, each of which must be the
    /// one given.
    Equal(&'static [(&'static str, i64)]),
    /// By the integer that its final call returned under this name, which must be above 0.
    Positive(&'static str),
    /// By the script below `examples/`, given the record and then these arguments.
    Script(&'static str, &'static [&'static str]),
    /// By the smoke's probes, of its sandboxes on CPUs.
    Probes,
}

/// A run of an example: its name, its entry below `examples/`, its `--arg` values and its check.
struct Example {
    name: &'static str,
    task: &'static str,
    args: Vec<String>,
    check: Check,
}

/// The runs, the native benchmark's first, as it runs longest. Those of a model run on `model`.
fn entries(model: &str) -> Vec<Example> {
    let model = format!("model={model}");
    let example = |name, task, args: &[&str], check| Example {
        name,
        task,
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        check,
    };
    let team = "team/verify.py";
    // `echo hello` writes 6 bytes, a line.
    let hello = Check::Equal(&[("score", 6), ("lines", 1)]);
    vec![
        example(
            "native",
            "hypergraph-benchmark/task.star",
            &[],
            Check::Script("hypergraph-benchmark/verify.py", &[]),
        ),
        example("hello", "hello/task.star", &[], hello),
        example(
            "codex",
            "codex/task.star",
            &[&model],
            Check::Positive("score"),
        ),
        example(
            "hypergraph",
            "hypergraph/task.star",
            &[&model],
            Check::Script("hypergraph/verify.py", &[]),
        ),
        example("team", "team/task.star", &[], Check::Script(team, &[])),
        example(
            "team-omit-bob",
            "team/task.star",
            &["omit=bob"],
            Check::Script(team, &["--omit", "bob"]),
        ),
        example(
            "codex-team",
            "team/codex.star",
            &[&model],
            Check::Script(team, &[]),
        ),
        example("smoke", "smoke/task.star", &[], Check::Probes),
    ]
}

/// The native benchmark's binding, which stays outside the repository: the file, which
/// `prepare.py` checks against its hash, the `revision` of the public source that its `source`
/// names, and the Git `repository` of that source.
struct Binding {
    file: PathBuf,
    revision: String,
    repository: String,
}

impl Binding {
    /// Reads the binding at `file`, or fails saying to name it with `--binding`, for the public
    /// source in `repository`.
    fn read(file: &Path, repository: &str) -> Result<Self> {
        let file = std::path::absolute(file)
            .map_err(|error| format!("making the binding's path absolute failed: {error}"))?;
        let given = "give the native benchmark's binding with `--binding`";
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("reading {} failed: {error}: {given}", file.display()))?;
        let binding: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("{} isn't JSON: {error}: {given}", file.display()))?;
        let Some(revision) = binding["source"]["revision"].as_str() else {
            let shown = file.display();
            return Err(format!("{shown}'s `source` names no `revision`: {given}").into());
        };
        Ok(Self {
            revision: revision.to_owned(),
            repository: repository.to_owned(),
            file,
        })
    }
}

/// A run that goes, followed by `./pumpkin run --wait` into its log.
struct Going {
    index: usize,
    example: Example,
    log: PathBuf,
    child: Child,
    started: Instant,
}

/// How a run ended, as its summary line says it.
struct Ran {
    index: usize,
    name: &'static str,
    id: Option<String>,
    /// The `state` of its `outcome.json`.
    state: String,
    checked: &'static str,
    elapsed: Duration,
    usd: Option<f64>,
    failed: Vec<String>,
}

impl Ran {
    /// The run that `passed` kept of the example `name`, the `index`th.
    fn kept(index: usize, name: &'static str, passed: resume::Passed) -> Self {
        Self {
            index,
            name,
            id: Some(passed.id),
            state: "finished".to_owned(),
            checked: "passed",
            elapsed: passed.elapsed,
            usd: passed.usd,
            failed: Vec::new(),
        }
    }

    /// The example `name`, the `index`th, that started no run, for what is in `failed`.
    fn unstarted(index: usize, name: &'static str, elapsed: Duration, failed: Vec<String>) -> Self {
        Self {
            index,
            name,
            id: None,
            state: "unstarted".to_owned(),
            checked: "not run",
            elapsed,
            usd: None,
            failed,
        }
    }
}

/// The directories that the runs of a commit keep their files in, below the target directory.
struct Dirs {
    /// The task repository, of `examples/` at the commit.
    repo: PathBuf,
    /// The log of each example's latest run, and those that it ran before.
    logs: PathBuf,
    /// What [`resume`] keeps of each example that passed.
    passed: PathBuf,
}

/// Runs each shipped example at `HEAD` through the controller of `config`, or of `pumpkin`'s
/// default configuration, those of a model on `model`, the native benchmark's from the revision
/// that the file `binding` names of the public source in the Git repository `source`, and fails
/// unless each run finished, its checker passed, every sandbox of it was deleted and the
/// providers hold nothing of it. Run again on a commit, it runs only the examples that haven't
/// passed there, and keeps the others. Before those runs it fails if `pumpkin-admin check` finds
/// a problem with the deployment. With `status`, posts how they ended as the commit status
/// [`gate::CONTEXT`] of `HEAD` on GitHub.
pub(super) fn examples(
    config: Option<&Path>,
    model: &str,
    binding: &Path,
    source: &str,
    status: bool,
) -> Result<()> {
    let commit = head_commit()?;
    let binding = Binding::read(binding, source)?;
    let config = config
        .map(std::path::absolute)
        .transpose()
        .map_err(|error| format!("making the configuration's path absolute failed: {error}"))?;
    let settings = settings(config.as_deref(), None, None)?;
    reached(&settings)?;
    tools_reached(&commit, status)?;
    let again = again(model, &binding, config.as_deref(), status);
    let dirs = repository(&commit, &binding)?;
    let (mut runs, mut waiting) = (Vec::new(), Vec::new());
    for (index, example) in entries(model).into_iter().enumerate() {
        match resume::read(&dirs.passed, example.name, &example.args)? {
            Some(passed) => {
                eprintln!(
                    "xtask: kept {}, which passed as run {}",
                    example.name, passed.id
                );
                runs.push(Ran::kept(index, example.name, passed));
            }
            None => waiting.push((index, example)),
        }
    }
    if !waiting.is_empty() {
        // Before the paid runs, so that a problem with the deployment costs nothing.
        checked_deployment(&settings, &dirs.repo, &again)?;
        eprintln!(
            "xtask: running {} of the examples at {commit} through the controller, {AT_ONCE} at \
             once, each followed in its log in {}",
            waiting.len(),
            dirs.logs.display()
        );
    }
    let started = Instant::now();
    runs.extend(run(&settings, &dirs, config.as_deref(), waiting));
    runs.sort_by_key(|ran| ran.index);
    let (failed, state, description) = summary(&runs, started.elapsed());
    let posted = if status {
        post(&commit, state, &description)
    } else {
        Ok(())
    };
    if !failed.is_empty() {
        let posted = match posted {
            Ok(()) if status => format!("; posted `{}` as failed", gate::CONTEXT),
            Ok(()) => String::new(),
            Err(error) => format!("; posting `{}` failed: {error}", gate::CONTEXT),
        };
        return Err(format!(
            "{}{posted}: fix it, and run `{again}` again, which runs only the examples that \
             failed",
            failed.join("; ")
        )
        .into());
    }
    posted?;
    println!(
        "xtask: every run finished, its checker passed, and nothing of it is left{}",
        if status { "; posted it on GitHub" } else { "" }
    );
    Ok(())
}

/// The command that runs the examples again as they ran with `model`, `binding`, `config` and
/// `status`.
fn again(model: &str, binding: &Binding, config: Option<&Path>, status: bool) -> String {
    let Binding {
        file,
        repository,
        revision: _,
    } = binding;
    let mut again = format!("cargo xtask live examples --model {model}");
    again = format!("{again} --binding {} --source {repository}", file.display());
    if let Some(config) = config {
        again = format!("{again} --config {}", config.display());
    }
    if status {
        again.push_str(" --status");
    }
    again
}

/// Fails, saying what to give or install, unless `zstd`, which the runs need, is there and, with
/// `status`, GitHub has `commit` for the `gh` that will post its status.
fn tools_reached(commit: &str, status: bool) -> Result<()> {
    if status {
        let github = format!("repos/{{owner}}/{{repo}}/commits/{commit}");
        let found = read(
            program("gh", root()).args(["api", "--silent", &github]),
            STEP_TIMEOUT,
        );
        found.map_err(|error| {
            format!(
                "{error}: push {commit} to GitHub, and give `gh` a token that may post its \
                 statuses, as `gh auth login` does, then run this again"
            )
        })?;
    }
    // The native benchmark's checker reads its record's compressed files with it.
    let zstd = read(program("zstd", root()).arg("--version"), STEP_TIMEOUT);
    zstd.map_err(|error| format!("{error}: install zstd, and run this again"))?;
    Ok(())
}

/// Makes the task repository of the runs at `commit`, below the target directory, a Git
/// repository of `examples/` at `commit` with `pumpkin` built from this checkout and the native
/// benchmark's inputs prepared, unless an earlier run of the commit made it, and returns its
/// directories.
fn repository(commit: &str, binding: &Binding) -> Result<Dirs> {
    let live = target_dir()?.join("live");
    let dir = live.join("examples").join(commit);
    let dirs = Dirs {
        repo: dir.join("repo"),
        logs: dir.join("logs"),
        passed: dir.join("passed"),
    };
    // `passed` is made last, so a directory without it is of a run that stopped before the
    // repository was ready, and no run of an example has started in it.
    if dirs.passed.is_dir() {
        return Ok(dirs);
    }
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .map_err(|error| format!("removing {} failed: {error}", dir.display()))?;
    }
    fs::create_dir_all(&dirs.logs).map_err(|error| made(&dirs.logs, &error))?;
    fs::create_dir(&dirs.repo).map_err(|error| made(&dirs.repo, &error))?;
    Repo { root: &dirs.repo }.git(["init", "--quiet"])?;
    let archive = dir.join("examples.tar");
    let output = format!("--output={}", archive.display());
    let checkout = Repo { root: root() };
    checkout.git(["archive", "--format=tar", &output, commit, "examples"])?;
    tools::unpack(&archive, &dirs.repo)?;
    install(&dirs.repo)?;
    prepare(&dirs.repo, &live.join("benchmark-source"), binding)?;
    fs::create_dir(&dirs.passed).map_err(|error| made(&dirs.passed, &error))?;
    Ok(dirs)
}

/// Prints a line of each of `runs`, which took `elapsed`, and of their total, and returns what
/// failed and the commit status that says how they ended, its state and description.
fn summary(runs: &[Ran], elapsed: Duration) -> (Vec<String>, &'static str, String) {
    for ran in runs {
        println!(
            "xtask: {:<13} {:<24} {:<11} checker {:<9} {:>8} {:>10}",
            ran.name,
            ran.id.as_deref().unwrap_or("-"),
            ran.state,
            ran.checked,
            shown(ran.elapsed),
            ran.usd.map_or("-".to_owned(), |usd| format!("${usd:.4}")),
        );
    }
    let usd: f64 = runs.iter().filter_map(|ran| ran.usd).sum();
    let failed: Vec<String> = (runs.iter())
        .flat_map(|ran| (ran.failed.iter()).map(|failed| format!("{}: {failed}", ran.name)))
        .collect();
    let (count, failed_count) = (
        runs.len(),
        runs.iter().filter(|ran| !ran.failed.is_empty()).count(),
    );
    println!(
        "xtask: {} of {count} runs passed, in {}, ${usd:.4} metered",
        count - failed_count,
        shown(elapsed)
    );
    let native = (runs.iter())
        .find(|ran| ran.name == "native")
        .and_then(|ran| ran.id.as_deref())
        .unwrap_or("none");
    let (state, ended) = if failed.is_empty() {
        ("success", format!("{count} runs passed"))
    } else {
        ("failure", format!("{failed_count} of {count} runs failed"))
    };
    (
        failed,
        state,
        format!("{ended}, ${usd:.2} metered; native run {native}"),
    )
}

/// Fetches the native benchmark's public source, which `binding` names, into `source`, cloned
/// there blobless once, and prepares its inputs in `repo` from it.
fn prepare(repo: &Path, source: &Path, binding: &Binding) -> Result<()> {
    if !source.exists() {
        let mut clone = program("git", repo);
        clone.args(["clone", "--quiet", "--filter=blob:none", "--no-checkout"]);
        clone.args(["--depth=1", &binding.repository]).arg(source);
        ran(&mut clone, step(STEP_TIMEOUT))?;
    }
    // Git fetches each blob that preparation reads as it reads it.
    let mut fetch = program("git", source);
    fetch.args(["fetch", "--quiet", "--filter=blob:none", "--depth=1"]);
    ran(
        fetch.args(["origin", &binding.revision]),
        step(STEP_TIMEOUT),
    )?;
    // `-B`, so that no bytecode joins the files of the task repository that Git lists.
    let mut prepare = program("python3", repo);
    prepare.arg("-B");
    prepare.arg(repo.join("examples/hypergraph-benchmark/prepare.py"));
    ran(prepare.arg(&binding.file).arg(source), step(STEP_TIMEOUT))?;
    Ok(())
}

/// Runs `examples`, each with its index, in `dirs`, [`AT_ONCE`] at a time, in their order, through
/// the controller of `config`, each followed into its log, and returns how each ended, in the
/// order they ended. Each that passed is kept, as soon as it has.
fn run(
    settings: &Settings,
    dirs: &Dirs,
    config: Option<&Path>,
    examples: Vec<(usize, Example)>,
) -> Vec<Ran> {
    let mut waiting = examples.into_iter();
    let mut going: Vec<Going> = Vec::new();
    let mut runs = Vec::new();
    loop {
        while going.len() < AT_ONCE
            && let Some((index, example)) = waiting.next()
        {
            let log = dirs.logs.join(format!("{}.log", example.name));
            let started = Instant::now();
            match spawn(&dirs.repo, &log, config, &example) {
                Ok(child) => {
                    eprintln!(
                        "xtask: started {}, followed in {}",
                        example.name,
                        log.display()
                    );
                    going.push(Going {
                        index,
                        example,
                        log,
                        child,
                        started,
                    });
                }
                Err(error) => runs.push(Ran::unstarted(
                    index,
                    example.name,
                    started.elapsed(),
                    vec![error],
                )),
            }
        }
        if going.is_empty() {
            return runs;
        }
        let mut children: Vec<&mut Child> =
            going.iter_mut().map(|going| &mut going.child).collect();
        let (ended, exited) = process::wait_any(&mut children);
        let exited = exited.map_err(|error| format!("waiting for `./pumpkin run` failed: {error}"));
        runs.push(finish(settings, dirs, going.swap_remove(ended), exited));
    }
}

/// Starts `./pumpkin run --wait` of `example` in `repo`, with what it says going to `log`.
fn spawn(
    repo: &Path,
    log: &Path,
    config: Option<&Path>,
    example: &Example,
) -> std::result::Result<Child, String> {
    let file = keep_earlier(log)
        .and_then(|()| File::create(log))
        .and_then(|file| Ok((file.try_clone()?, file)));
    let (stdout, stderr) = file.map_err(|error| made(log, &error))?;
    let mut run = pumpkin(repo);
    run.args(["run", "--wait"]);
    if let Some(config) = config {
        run.arg("--config").arg(config);
    }
    for arg in &example.args {
        run.args(["--arg", arg]);
    }
    run.arg(Path::new("examples").join(example.task));
    process::spawn(&mut run, Stdio::null(), stdout.into(), stderr.into())
}

/// Moves the log of an earlier run of the same example, if `log` is one, to `<name>.<n>.log` for
/// the first `n` that names none, so that the run that fails keeps its log when it is run again.
fn keep_earlier(log: &Path) -> std::io::Result<()> {
    if !log.exists() {
        return Ok(());
    }
    let mut attempt = 1;
    while log.with_extension(format!("{attempt}.log")).exists() {
        attempt += 1;
    }
    fs::rename(log, log.with_extension(format!("{attempt}.log")))
}

/// How `going` ended, once `./pumpkin run --wait` has exited as `exited` says: its record read
/// and checked, its checker run, which adds what it says to the log, and the providers asked what
/// they hold of it. A run that passed is kept in `dirs`.
fn finish(
    settings: &Settings,
    dirs: &Dirs,
    going: Going,
    exited: std::result::Result<ExitStatus, String>,
) -> Ran {
    let repo = &dirs.repo;
    let Going {
        index,
        example,
        log,
        child: _,
        started,
    } = going;
    let elapsed = started.elapsed();
    let mut failed = Vec::new();
    match exited {
        Ok(status) if status.success() => {}
        Ok(status) => failed.push(format!(
            "`./pumpkin run` ended with {status}, as {} says",
            log.display()
        )),
        Err(error) => failed.push(error),
    }
    let said = fs::read_to_string(&log).unwrap_or_else(|error| {
        failed.push(format!("reading {} failed: {error}", log.display()));
        String::new()
    });
    let id = said.lines().find_map(wait::run_started).map(str::to_owned);
    let Some(run) = &id else {
        failed.push(format!("it started no run, as {} says", log.display()));
        return Ran::unstarted(index, example.name, elapsed, failed);
    };
    let record = repo.join(".pumpkin/runs").join(run);
    let (outcome, problems) = ended(&record);
    failed.extend(problems);
    let problems = check(repo, &example.check, &record, &outcome, &log);
    let checked = if problems.is_empty() {
        "passed"
    } else {
        "failed"
    };
    failed.extend(problems);
    let mut ran = Ran {
        index,
        name: example.name,
        id,
        state: outcome["state"].as_str().unwrap_or("unread").to_owned(),
        checked,
        elapsed,
        usd: outcome["usage"]["usd"].as_f64(),
        failed,
    };
    leftovers(settings, repo, &mut ran);
    keep(dirs, &example, &mut ran);
    ran
}

/// Keeps `ran`, the run of `example`, in `dirs` if it passed, or adds to what it failed why it
/// can't be kept.
fn keep(dirs: &Dirs, example: &Example, ran: &mut Ran) {
    if !ran.failed.is_empty() {
        return;
    }
    let Some(id) = ran.id.clone() else {
        return;
    };
    let passed = resume::Passed {
        id,
        args: example.args.clone(),
        elapsed: ran.elapsed,
        usd: ran.usd,
    };
    if let Err(error) = resume::save(&dirs.passed, example.name, &passed) {
        ran.failed.push(error);
    }
}

/// What is wrong in the record at `record`, whose `outcome.json` is `outcome`, by `check`, and
/// what a checker says, which goes to `log`.
fn check(
    repo: &Path,
    check: &Check,
    record: &Path,
    outcome: &serde_json::Value,
    log: &Path,
) -> Vec<String> {
    let returned = |name: &str| {
        let values = outcome["verdict"]["result"]["values"].as_array();
        let value = (values.into_iter().flatten()).find(|value| value["name"] == name);
        value.and_then(|value| value["value"].as_i64())
    };
    let got = |name: &str| returned(name).map_or("nothing".to_owned(), |got| got.to_string());
    match check {
        Check::Equal(expected) => (expected.iter())
            .filter(|&&(name, expected)| returned(name) != Some(expected))
            .map(|(name, expected)| format!("its `{name}` is {}: expected {expected}", got(name)))
            .collect(),
        Check::Positive(name) if returned(name).is_some_and(|got| got > 0) => Vec::new(),
        Check::Positive(name) => vec![format!(
            "its `{name}` is {}: expected one above 0",
            got(name)
        )],
        Check::Probes => probes(outcome, false),
        Check::Script(script, args) => {
            let script = repo.join("examples").join(script);
            checker(&script, record, args, log)
                .err()
                .into_iter()
                .collect()
        }
    }
}

/// Adds to what `ran` failed what of its run the providers that `settings` reach still hold, as
/// [`left`] finds it, each of which `./pumpkin cancel` in `repo` removes.
fn leftovers(settings: &Settings, repo: &Path, ran: &mut Ran) {
    let Some(id) = &ran.id else {
        return;
    };
    match left(settings, id) {
        Ok(left) if left.is_empty() => {}
        Ok(left) => ran.failed.push(format!(
            "left after the run: {}; remove it with `./pumpkin cancel {id}` in {}",
            left.join(", "),
            repo.display()
        )),
        Err(error) => ran
            .failed
            .push(format!("finding what is left of run {id} failed: {error}")),
    }
}

/// Runs `script` on the record at `record`, with `args` after it, adding what it says to `log`.
fn checker(
    script: &Path,
    record: &Path,
    args: &[&str],
    log: &Path,
) -> std::result::Result<(), String> {
    let mut check = program("python3", root());
    check.arg("-B").arg(script).arg(record).args(args);
    let line = process::shown(&check);
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
    let (output, result) = match process::run(&mut check, how) {
        Ok(output) => (output, Ok(())),
        Err(Failure::Exited { output, .. }) => {
            let ended = format!(
                "`{line}` ended with {}, as {} says",
                output.status,
                log.display()
            );
            (output, Err(ended))
        }
        Err(failure) => return Err(failure.to_string()),
    };
    let mut file = File::options()
        .append(true)
        .open(log)
        .map_err(|error| made(log, &error))?;
    let written = file
        .write_all(&output.stdout)
        .and_then(|()| file.write_all(&output.stderr));
    written.map_err(|error| made(log, &error))?;
    result
}

/// Posts `state` with `description` as the commit status [`gate::CONTEXT`] of `commit`.
fn post(commit: &str, state: &str, description: &str) -> Result<()> {
    let statuses = format!("repos/{{owner}}/{{repo}}/statuses/{commit}");
    let mut post = program("gh", root());
    post.args(["api", "--silent", "--method", "POST", &statuses]);
    post.args(["--raw-field", &format!("state={state}")]);
    post.args(["--raw-field", &format!("context={}", gate::CONTEXT)]);
    post.args(["--raw-field", &format!("description={description}")]);
    read(&mut post, STEP_TIMEOUT)?;
    Ok(())
}

/// `elapsed` in minutes and seconds, as `86m52s`.
fn shown(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}
