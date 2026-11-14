//! The controller's own side of the controller target, as `pumpkin controller --help` says: a
//! job ([`job`]), which the client starts in a unit of its own, and begins with the prune
//! ([`prune`]), then builds its task's images as its [`Admission`] admits them, and the
//! controller's boot ([`boot`]).
//!
//! What they read and write lies below the controller user's home: the trees the client sent,
//! `trees/<repo>/<id>/`; each run's job in `jobs/<id>/`, and each build's in `builds/<id>/`,
//! with its configuration, `pumpkin.toml`, which holds no secret, and its unit's standard error
//! and output, `log` and `out`; the one workspace, `.pumpkin/` ([`crate::workspace`]), that
//! every tree's `.pumpkin` links to, where each job's Pumpkin is built, which keeps the runs'
//! records in its runs directory ([`runs`]) and the build slots ([`Workspace::slots`]); and
//! `current-pumpkin`, the `pumpkin` that the last job ran. What the client sent with a job
//! ([`SENT_CONFIG`], [`SENT_MODAL`] and the upstreams' key files) is in the job's directory
//! below `$XDG_RUNTIME_DIR/pumpkin/` ([`jobs`]), a tmpfs that the job's unit removes as it
//! ends, so a job that still runs is one whose directory is there, as is what images the job
//! holds ([`HELD`]), so what a job holds goes with it, however it ends.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::os::unix::fs::{OpenOptionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use aws::{Bound, Cli};
use engine::Engine;
use engine::prune::{Disk, HIGH_PERCENT, Held, REFUSED_PERCENT};
use provider::RunTag;
use runtime::Record;
use schema::ir::Image;
use serde::Deserialize;
use stdx::Context as _;
use stdx::process::Environment;

use crate::controller::bounded;
use crate::workspace::{self, Workspace};
use crate::{cli, say, star};

/// The directory of the runs' jobs, below the home.
pub(crate) const JOBS: &str = "jobs";

/// The directory of the builds' jobs, below the home.
pub(crate) const BUILDS: &str = "builds";

/// The directory of the trees that the client sent, below the home.
pub(crate) const TREES: &str = "trees";

/// The link to the `pumpkin` that the last job ran, below the home, which the controller's boot
/// runs.
pub(crate) const CURRENT: &str = "current-pumpkin";

/// The runs directory of the controller's one workspace, below the home: that of every job's
/// `pumpkin`, as each is built in that workspace.
pub(crate) fn runs() -> String {
    format!("{}/{}", workspace::DIR, workspace::RUNS)
}

/// The link, in a job's directory, to the `pumpkin` that the job runs.
pub(crate) const OWN: &str = "pumpkin";

/// The exit status of `pumpkin controller follow` ([`follow`]) and `pumpkin controller cancel`
/// ([`cancel`]) once the run has ended, whose ending the client says from its copy of the record.
pub(crate) const ENDED: u8 = 102;

/// The exit status of `pumpkin controller cancel` once the run has ended, but its provider
/// couldn't be reached to remove what it left.
pub(crate) const ENDED_UNREACHED: u8 = 103;

/// The file that holds what the client sent of its configuration ([`Sent`]).
pub(crate) const SENT_CONFIG: &str = "config.toml";

/// The copy of the client's Modal configuration file, which the job's Modal providers read.
pub(crate) const SENT_MODAL: &str = "modal.toml";

/// The file, beside what the client sent, of what images the job holds, a line each, by its tag
/// or its ID, which no prune removes while the job runs ([`Admission`]).
const HELD: &str = "held";

/// The file, beside what the client sent, that is there while the job holds a build slot, so
/// may build an image that no tag names yet, which no prune then removes ([`Slot`]).
const BUILDING: &str = "building";

/// The file, beside what the client sent, that is there while the job waits for a build slot
/// ([`take_slot`]), so `ls` lists it as waiting.
pub(crate) const WAITING: &str = "waiting";

/// How long a job's tree is kept after its start.
// A run's record names its entry in its tree, to run it again, and a tree is mostly links to
// the files of the one before it that are unchanged.
const KEPT: Duration = Duration::from_hours(14 * 24);

/// A path on the controller's one volume, which holds the engine's images and build cache, the
/// trees and the records, as `deploy/aws` makes it.
const VOLUME: &str = "/";

/// How long `df` may take to say how much of the volume is used.
const DF_TIMEOUT: Duration = Duration::from_secs(10);

/// The instance metadata service, as its version 2, `IMDSv2`, serves it.
const IMDS: &str = "http://169.254.169.254/latest";

/// How long `curl` may take to answer from the instance metadata service, which is the
/// instance's own.
const IMDS_SECONDS: u64 = 10;

/// Runs the job that `job` asks for: points [`CURRENT`] at this `pumpkin`, writes the job's
/// configuration ([`assemble`]), prunes, builds its task's images ([`build_first`]), then runs
/// its command with it, a run to its end, which finds each image by its tag; returns the exit
/// status of its command.
///
/// # Errors
///
/// Fails if the job's directory, its configuration or [`CURRENT`] can't be made, its task's
/// images can't be built, or its command fails before it starts.
pub(crate) fn job(job: cli::Job) -> Result<ExitCode, String> {
    let cli::Job {
        id,
        region,
        parameter,
        command,
    } = job;
    let home = crate::home("run the job as the controller's user")?;
    let jobs = jobs()?;
    let sent = jobs.join(id.as_str());
    point_current(&home, &id)?;
    let (kind, entry, args) = match &command {
        cli::JobCommand::Run(run) => (JOBS, &run.entry, &run.args.0),
        cli::JobCommand::Build(build) => (BUILDS, &build.entry, &build.args.0),
    };
    let dir = home.join(kind).join(id.as_str());
    fs::create_dir_all(&dir).context(dir.display())?;
    // So that a client that follows the run has it followed by the `pumpkin` it sent the job
    // with, as `current-pumpkin` may be another job's by then.
    point(&dir.join(OWN), &dir.join(format!(".{OWN}")))?;
    let assembled = assemble(&Cli::connect(region)?, &parameter, &sent)?;
    let path = dir.join("pumpkin.toml");
    fs::write(&path, assembled).context(path.display())?;
    let Some(config) = path.to_str().map(str::to_owned) else {
        return Err(format!(
            "{} isn't UTF-8, as a run's record keeps it: give the controller's user a home \
             whose path is",
            path.display()
        ));
    };
    // Before the build, so that it has room.
    if let Err(error) = prune(&home, &path, &jobs) {
        say(format_args!("the prune at the job's start failed: {error}"));
    }
    build_first(&id, &home, &path, &jobs, entry, args)?;
    let exit = match command {
        cli::JobCommand::Run(run) => {
            let run = cli::Run {
                config: Some(config),
                wait: false,
                ..run
            };
            // Only one who follows the run reads the job's log.
            let exit = crate::start(&id, run, crate::Reader::Follower)?;
            // The run's end is the job's, so that the unit lasts as long as the run.
            if exit == ExitCode::SUCCESS {
                let runs = crate::default_runs()?;
                Record::open(&Path::new(&runs).join(id.as_str()))?.wait(|| {})?;
            }
            exit
        }
        cli::JobCommand::Build(build) => crate::build(
            cli::Build {
                config: Some(config),
                ..build
            },
            crate::Reader::Follower,
        )?,
    };
    Ok(exit)
}

/// The controller's boot: reaps what each run that has ended or is lost, but one that ended
/// clean, may have left ([`crate::reap`], [`crate::Reaper::Boot`]), with the configuration of
/// its job; returns the reap's exit status, 0 though a provider can't be reached.
///
/// # Errors
///
/// Fails if the reap can't begin, as without a runs directory.
pub(crate) fn boot() -> Result<ExitCode, String> {
    // No prune, which each job's start makes: a client may start a job as soon as SSM sees the
    // controller, before the boot is done, and the boot, a unit of the system's, can't see that
    // it runs, as systemd names the user's runtime directory, which holds each job's directory,
    // to the user's own units alone. A prune here would remove the images that the job has
    // built, which no IR that the boot reads names yet.
    let reap = cli::Reap {
        runs: None,
        modal_config: None,
    };
    crate::reap(reap, crate::Reaper::Boot)
}

/// Follows run `id` for a client, as `pumpkin wait` does, saying its milestones until it ends,
/// but none of its ending; then exits [`ENDED`].
///
/// # Errors
///
/// Fails if the run's record can't be read.
pub(crate) fn follow(id: &RunTag) -> Result<ExitCode, String> {
    let runs = crate::default_runs()?;
    let record = Record::open(&Path::new(&runs).join(id.as_str()))?;
    if record.ending()?.is_none() {
        // Of the client's `wait` or `run --wait`, which it doesn't know, and whose record is the
        // copy that the client names as it ends.
        say(format_args!(
            "following run {}: ending this command does not stop it",
            record.id()
        ));
        crate::followed(&record)?;
    }
    Ok(ExitCode::from(ENDED))
}

/// Cancels the run that `flags` name for a client, as `pumpkin cancel` does, but says none of
/// its ending; then exits [`ENDED`], or [`ENDED_UNREACHED`] if its provider couldn't be reached.
///
/// # Errors
///
/// As `pumpkin cancel` fails before the run's ending.
pub(crate) fn cancel(flags: cli::Cancel) -> Result<ExitCode, String> {
    let (_, _, reached) = crate::cancelled(flags, crate::Reader::Follower)?;
    Ok(ExitCode::from(if reached {
        ENDED
    } else {
        ENDED_UNREACHED
    }))
}

/// The directory of every job's directory of what the client sent with it,
/// `$XDG_RUNTIME_DIR/pumpkin/`, each named by its job's id.
///
/// # Errors
///
/// Fails if `XDG_RUNTIME_DIR` is unset or empty, as outside a user's systemd unit.
fn jobs() -> Result<PathBuf, String> {
    // Every job's: the user's systemd gives each job's unit the user's runtime directory, as
    // pam_systemd gives the session that makes the job's directory there.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty());
    let Some(runtime) = runtime else {
        return Err(
            "XDG_RUNTIME_DIR is unset or empty: run the job in a unit of the user's systemd, as \
             the controller target starts it"
                .to_owned(),
        );
    };
    Ok(Path::new(&runtime).join("pumpkin"))
}

/// What the jobs that run hold, of the directories of every job in `jobs`, and their runs'
/// records in `runs`: the images that the IR of each run that has started names, whose
/// record has its run log, which a run begins only once it has published every image it runs;
/// each image or tag in a job's [`HELD`]; and whether one is [`BUILDING`]. A job that runs is
/// one whose directory is there, as a unit's end removes its directory.
fn held(jobs: &Path, runs: &Path) -> Result<Held, String> {
    let mut held = Held::default();
    for id in ids(jobs)? {
        let job = jobs.join(&id);
        // Gone, if the job has ended since it was listed.
        let named = job.join(HELD);
        match fs::read_to_string(&named) {
            Ok(named) => {
                for line in named.lines() {
                    match Image::try_from(line.to_owned()) {
                        Ok(image) => held.images.insert(image),
                        Err(_) => held.tags.insert(line.to_owned()),
                    };
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", named.display())),
        }
        let building = job.join(BUILDING);
        held.building |= building.try_exists().context(building.display())?;
        let record = runs.join(&id);
        let log = record.join("log.jsonl");
        if log.try_exists().context(log.display())? {
            held.images.extend(images(&record.join("ir.json"))?);
        }
    }
    Ok(held)
}

/// The directory of every job's directory in tmpfs, `jobs`, opened to be locked: a prune
/// locks it while it reads what the jobs hold and removes what they don't ([`collect`]), and a
/// job shares the lock while it adds to what it holds ([`Admission`]), so that no image a job
/// has held before it looks it up is removed while the job runs.
fn open_jobs(jobs: &Path) -> Result<File, String> {
    File::open(jobs).context(jobs.display())
}

/// Points [`CURRENT`] in `home` at this `pumpkin`, which job `id` runs.
fn point_current(home: &Path, id: &RunTag) -> Result<(), String> {
    let staged = home.join(format!(".{CURRENT}.{}", id.as_str()));
    point(&home.join(CURRENT), &staged)
}

/// Points `link` at this `pumpkin`, replacing what it pointed at whole, by way of `staged`, a
/// path beside it that no other job uses.
fn point(link: &Path, staged: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().context("finding this `pumpkin`'s executable failed")?;
    symlink(&exe, staged).context(staged.display())?;
    fs::rename(staged, link).context(link.display())
}

/// What `aws ssm get-parameter` prints.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GotParameter {
    parameter: Parameter,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Parameter {
    value: String,
}

/// What the client sent of its configuration ([`crate::config::Client::sent`]): its `[upstreams]`
/// tables, each `key_file` a file of the job's directory in tmpfs, and its Modal provider's
/// settings, if it names one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sent {
    #[serde(default)]
    upstreams: toml::Table,
    #[serde(default)]
    modal: Option<toml::Table>,
}

/// A job's configuration, as TOML: the controller's settings, which `cli` reads from the SSM
/// parameter `parameter`, with `[gateway] private` at the controller's private IPv4 address,
/// port 0; each Modal provider as the client sent it, with the settings' `images`, `dials_from`
/// at the controller's public IPv4 address, and `config`, the client's Modal file in `sent`, if
/// any; and the client's `[upstreams]`, each `key_file` a file in `sent`.
///
/// # Errors
///
/// Fails if the parameter or metadata can't be read, or what either holds is not as above.
fn assemble(cli: &Cli, parameter: &str, sent: &Path) -> Result<String, String> {
    let got: GotParameter =
        cli.json(&["ssm", "get-parameter", "--name", parameter], Bound::Call)?;
    let named = format!("the controller's settings, SSM parameter `{parameter}`,");
    let mut tables: toml::Table = toml::from_str(&got.parameter.value)
        .map_err(|error| format!("{named} aren't TOML: {}", error.to_string().trim_end()))?;
    let (private, public) = addresses()?;
    for key in ["gateway", "upstreams"] {
        if tables.contains_key(key) {
            return Err(format!(
                "{named} hold `[{key}]`, which each job sets: remove it from them"
            ));
        }
    }
    let gateway = toml::Table::from_iter([("private".to_owned(), format!("{private}:0").into())]);
    tables.insert("gateway".to_owned(), gateway.into());
    let config = sent.join(SENT_CONFIG);
    let text = match fs::read_to_string(&config) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", config.display())),
    };
    let Sent {
        mut upstreams,
        mut modal,
    } = toml::from_str(&text).map_err(|error| {
        let error = error.to_string();
        format!(
            "what the run sent, {SENT_CONFIG}, is refused: {}",
            error.trim_end()
        )
    })?;
    let modal_config = sent.join(SENT_MODAL);
    let modal_config = modal_config
        .try_exists()
        .context(modal_config.display())?
        .then(|| modal_config.to_string_lossy().into_owned());
    if let Some(modal) = &mut modal {
        modal.insert("dials_from".to_owned(), public.to_string().into());
        if let Some(config) = modal_config {
            modal.insert("config".to_owned(), config.into());
        }
    }
    take_modal(&mut tables, modal.as_ref(), &named)?;
    for (name, upstream) in &mut upstreams {
        let file = (upstream.get("key_file").and_then(toml::Value::as_str)).map(PathBuf::from);
        let file = file.filter(|file| file.file_name() == Some(file.as_os_str()));
        let (Some(file), Some(upstream)) = (file, upstream.as_table_mut()) else {
            return Err(format!(
                "what the run sent gives upstream `{name}` no `key_file` that names a file it \
                 sent: send each key as a file of its own"
            ));
        };
        let file = sent.join(file).to_string_lossy().into_owned();
        upstream.insert("key_file".to_owned(), file.into());
    }
    tables.insert("upstreams".to_owned(), upstreams.into());
    toml::to_string(&tables).map_err(|error| format!("writing the job's configuration: {error}"))
}

/// Makes each Modal provider of the controller's settings, `tables`, the one that the client
/// sent, `sent`, with the settings' `images`, or removes it if the client sent none.
///
/// # Errors
///
/// Fails, naming the settings as `named`, if the client sent one, but they name none.
fn take_modal(
    tables: &mut toml::Table,
    sent: Option<&toml::Table>,
    named: &str,
) -> Result<(), String> {
    // What a Modal workspace offers is the client's to say, so that the person changes it with
    // no apply; the controller's settings give the provider only what the module made.
    let providers = tables
        .get_mut("providers")
        .and_then(toml::Value::as_table_mut);
    let Some(sent) = sent else {
        if let Some(providers) = providers {
            providers
                .retain(|_, kinds| kinds.get("modal").and_then(toml::Value::as_table).is_none());
        }
        return Ok(());
    };
    let modal: Vec<&mut toml::Table> = (providers.into_iter())
        .flat_map(|providers| providers.iter_mut().map(|(_, kinds)| kinds))
        .filter_map(|kinds| kinds.get_mut("modal")?.as_table_mut())
        .collect();
    if modal.is_empty() {
        return Err(format!(
            "{named} name no Modal provider, whose `images` the client's `[controller.modal]` \
             needs: apply `deploy/aws`, whose settings name one"
        ));
    }
    for settings in modal {
        let images = settings.remove("images");
        *settings = sent.clone();
        if let Some(images) = images {
            settings.insert("images".to_owned(), images);
        }
    }
    Ok(())
}

/// The instance's private and public IPv4 addresses, as the instance metadata service serves
/// them, `local-ipv4` and `public-ipv4`, under an `IMDSv2` token, through `curl`.
fn addresses() -> Result<(Ipv4Addr, Ipv4Addr), String> {
    // Long enough for the two reads that follow.
    let ttl = "X-aws-ec2-metadata-token-ttl-seconds: 60";
    let token = curl(&[
        "--request",
        "PUT",
        "--header",
        ttl,
        &format!("{IMDS}/api/token"),
    ])?;
    let token = format!("X-aws-ec2-metadata-token: {token}");
    let address = |item: &str| {
        let address = curl(&["--header", &token, &format!("{IMDS}/meta-data/{item}")])?;
        address.parse().map_err(|_| {
            format!(
                "the instance metadata's `{item}` is `{}`, which is no IPv4 address: expected \
                 the controller's, as its subnet gives it",
                address.escape_debug()
            )
        })
    };
    Ok((address("local-ipv4")?, address("public-ipv4")?))
}

/// What `curl` prints with `args`, after the options that make it read no configuration file,
/// fail on an HTTP error, and give up after [`IMDS_SECONDS`].
fn curl(args: &[&str]) -> Result<String, String> {
    // Only `PATH`, so that no proxy variable sends it elsewhere.
    let mut curl = stdx::process::command("curl", Environment::Only(&["PATH"]));
    let seconds = IMDS_SECONDS.to_string();
    curl.args([
        "--disable",
        "--silent",
        "--show-error",
        "--fail",
        "--max-time",
        &seconds,
    ]);
    curl.args(args);
    let shown = format!("curl {}", args.last().copied().unwrap_or_default());
    let timeout = Duration::from_secs(2 * IMDS_SECONDS);
    let printed = bounded(&mut curl, &shown, timeout).map_err(|error| {
        format!("{error}: expected the instance metadata service to answer, as on an EC2 instance")
    })?;
    Ok(String::from_utf8_lossy(&printed).trim().to_owned())
}

/// The ids below `dir`, each a job's or a tree's, by name, which sort as their starts do; none
/// if `dir` isn't there.
fn ids(dir: &Path) -> Result<Vec<String>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let name = entry.context(dir.display())?.file_name();
        // A tree's directory holds the shared `.pumpkin` and the `latest` link beside them.
        if let Some(name) = name
            .to_str()
            .filter(|name| name.starts_with(|c: char| c.is_ascii_digit()))
        {
            ids.push(name.to_owned());
        }
    }
    ids.sort();
    Ok(ids)
}

/// Prunes what the jobs keep on the controller's volume: removes each tree in `home` older than
/// [`KEPT`], and the directory of each run's job as old whose run has no record, which `ls`
/// lists until then; then, once the volume is [full](Disk::full), the images and the build cache
/// of the engine of the configuration at `config` ([`collect`]), sparing what the jobs in `jobs`
/// hold.
fn prune(home: &Path, config: &Path, jobs: &Path) -> Result<(), String> {
    let cutoff = crate::unix_now().saturating_sub(KEPT);
    let old = |id: &String| runtime::minted(id).is_some_and(|minted| minted < cutoff);
    for repo in fs::read_dir(home.join(TREES)).into_iter().flatten() {
        let repo = repo.context("listing the trees failed")?.path();
        for id in ids(&repo)?.into_iter().filter(old) {
            let tree = repo.join(id);
            fs::remove_dir_all(&tree).context(tree.display())?;
        }
    }
    let runs = PathBuf::from(crate::default_runs()?);
    for id in ids(&home.join(JOBS))?.into_iter().filter(old) {
        // A run's record names its job's configuration, to run it again.
        let record = runs.join(&id);
        if !record.try_exists().context(record.display())? {
            let job = home.join(JOBS).join(id);
            fs::remove_dir_all(&job).context(job.display())?;
        }
    }
    // The engine is reached only once there may be something to remove.
    if !disk(VOLUME)?.full() {
        return Ok(());
    }
    collect(home, &engine(config)?, jobs)
}

/// The Docker engine that the configuration at `config` names, as a job reaches it, for its
/// follower ([`crate::Reader`]).
fn engine(config: &Path) -> Result<Engine, String> {
    crate::configured_engine(config, crate::Reader::Follower)
}

/// Removes the images of `engine` that `engine::prune::evicted` picks, each last used by the
/// newest job whose IR names it ([`last_used`]), sparing what the jobs that run hold
/// ([`held`]), as each image is once picked, then prunes the build cache down to the same mark,
/// all while the jobs' directory is locked ([`open_jobs`]), and only if the volume is
/// [full](Disk::full) once it is, so a prune that waited for another's lock removes only what
/// that one left.
fn collect(home: &Path, engine: &Engine, jobs: &Path) -> Result<(), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let last_used = last_used(home, &runs)?;
    // Before the lock, which the jobs' builds wait for: an image made since is in no list, and
    // one held since is in what is read under it.
    let images = engine.images()?;
    let locked = open_jobs(jobs)?;
    locked.lock().context(jobs_named(jobs))?;
    let disk = disk(VOLUME)?;
    if !disk.full() {
        return Ok(());
    }
    let held = held(jobs, &runs)?;
    let last_used = |image: &Image| last_used.get(image).cloned();
    let picked = engine::prune::evicted(disk, images, last_used, &held);
    // Picked by tags listed before the lock: an image may have been tagged since, as a build
    // that ended meanwhile tags what it built, so each is looked at again, now that nothing
    // that a job holds can change, and the pick is made again of what they are now.
    let evicted = engine::prune::evicted(disk, engine.inspected(&picked)?, last_used, &held);
    engine.remove_images(&evicted)?;
    say(format_args!(
        "the volume is more than {HIGH_PERCENT}% used: the prune removed {}, least recently used \
         first",
        crate::counted(evicted.len(), "image")
    ));
    engine.prune_build_cache(disk.low_free_bytes())
}

/// How the jobs' directory, `jobs`, is named in a failure to lock it.
fn jobs_named(jobs: &Path) -> String {
    format!("locking {} failed", jobs.display())
}

/// Builds the images of the task whose entry is `entry`, given `args`, for job `id`, on the
/// engine of the configuration at `config`, each build as the job's [`Admission`] admits it, so
/// that the job's command finds each by its tag; then, if it built one, lets go of the build
/// slot that the admission took, and prunes. What the job holds, it holds until it ends.
///
/// # Errors
///
/// As `star::checked` and the engine's reach fail; and gives the task's problems, or each build
/// that failed, said.
fn build_first(
    id: &RunTag,
    home: &Path,
    config: &Path,
    jobs: &Path,
    entry: &star::Entry,
    args: &lang::Args,
) -> Result<(), String> {
    let mut admission = Admission {
        id,
        home,
        jobs,
        slots: Workspace::find()?.slots(),
        slot: None,
    };
    let built = star::checked(entry, args, |task| {
        let engine = engine(config)?;
        let tags = match task.build(&engine, &mut admission) {
            Ok((_, tags)) => tags,
            Err(problems) => return Ok(Err(problems)),
        };
        // By their IDs too, until the job ends, as its run publishes them, however long it
        // waits to: their tags are held already, but another build of the same inputs may
        // move a tag to an image of its own.
        admission.held(tags.keys().map(Image::as_str))?;
        // A job that built took a slot, which goes before the prune, as a job that waits for it
        // needn't wait for that.
        if admission.slot.take().is_some() {
            admission.prune(&engine);
        }
        Ok(Ok(()))
    })?;
    built.map_err(|problems| crate::task_problems(entry, &problems))
}

/// What admits the builds of job `id` ([`star::Admission`]): it holds each image that they look
/// up, by its tag, in the job's [`HELD`] in its directory in `jobs`, sharing their lock
/// ([`open_jobs`]), and once its builds have ended, each image they found or built, by its ID;
/// takes one of the controller's build slots ([`take_slot`]) as the first build starts, and
/// keeps it until its holder lets go; and prunes ([`collect`]) as each build starts
/// and as each ends while others run, then refuses to start one while more than
/// [`REFUSED_PERCENT`] of the volume is used ([`Disk::refused`]).
struct Admission<'j> {
    id: &'j RunTag,
    home: &'j Path,
    jobs: &'j Path,
    /// The directory of the build slots.
    slots: PathBuf,
    slot: Option<Slot>,
}

impl Admission<'_> {
    /// The job's directory of what the client sent.
    fn sent(&self) -> PathBuf {
        self.jobs.join(self.id.as_str())
    }

    /// Prunes the images and the build cache of `engine`, once the volume is full, saying why
    /// the prune failed, if it did, as the build goes on.
    fn prune(&self, engine: &Engine) {
        let pruned = disk(VOLUME).and_then(|disk| {
            if disk.full() {
                collect(self.home, engine, self.jobs)
            } else {
                Ok(())
            }
        });
        if let Err(error) = pruned {
            say(format_args!("the prune failed: {error}"));
        }
    }

    /// Holds each of `named`, an image's tag or ID, in the job's [`HELD`], sharing the jobs'
    /// directory's lock.
    fn held<'n>(&self, named: impl Iterator<Item = &'n str>) -> Result<(), String> {
        let jobs = open_jobs(self.jobs)?;
        jobs.lock_shared().context(jobs_named(self.jobs))?;
        let path = self.sent().join(HELD);
        // As private as what the client sent beside it.
        let held = (OpenOptions::new().append(true).create(true))
            .mode(0o600)
            .open(&path);
        let mut lines = String::new();
        for named in named {
            lines.push_str(named);
            lines.push('\n');
        }
        let written = held.and_then(|mut held| held.write_all(lines.as_bytes()));
        written.context(path.display())
    }
}

impl engine::Admit for Admission<'_> {
    fn hold(&mut self, tag: &str) -> Result<(), String> {
        self.held([tag].into_iter())
    }

    fn admit(&mut self, engine: &Engine) -> Result<(), String> {
        if self.slot.is_none() {
            self.slot = Some(take_slot(&self.slots, self.id, &self.sent())?);
        }
        self.prune(engine);
        let disk = disk(VOLUME)?;
        if disk.refused() {
            let used_bytes = disk.size_bytes.saturating_sub(disk.free_bytes);
            let used = (used_bytes.saturating_mul(100))
                .checked_div(disk.size_bytes)
                .unwrap_or(100);
            return Err(format!(
                "the controller's volume is {used}% used, past the {REFUSED_PERCENT}% at which a \
                 build is refused, though the prune has removed what no job holds: free space \
                 there, or let the jobs that run end, then run the task again"
            ));
        }
        Ok(())
    }
}

impl star::Admission for Admission<'_> {
    fn ended(&mut self, engine: &Engine) {
        self.prune(engine);
    }
}

/// A build slot that a job holds, until it is dropped, with the job's [`BUILDING`], which goes
/// with it.
struct Slot {
    _locked: File,
    building: PathBuf,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // One left by a failure here goes with the job's directory as the job ends; until then,
        // it only spares untagged images, which no job may need.
        let _ = fs::remove_file(&self.building);
    }
}

/// One of the controller's build slots, files of `dir`, as many as [`slots_count`] gives, taken
/// as `stdx::slots::take` says, named by job `id`: saying, while each is held, which jobs hold
/// them, with [`WAITING`] beside `sent`, then how long it waited; then with [`BUILDING`] beside
/// `sent`.
fn take_slot(dir: &Path, id: &RunTag, sent: &Path) -> Result<Slot, String> {
    let count = slots_count();
    let marked = sent.join(WAITING);
    let mut waiting = None;
    let slot = stdx::slots::take(dir, count, id.as_str(), |holders| {
        waiting = Some(Instant::now());
        File::create(&marked).context(marked.display())?;
        say(format_args!(
            "waiting for a build slot: all {count} held, by jobs {}",
            holders.join(", ")
        ));
        Ok(())
    })?;
    if let Some(since) = waiting {
        fs::remove_file(&marked).context(marked.display())?;
        let waited = since.elapsed().as_secs();
        say(format_args!("took a build slot after waiting {waited} s"));
    }
    let building = sent.join(BUILDING);
    File::create(&building).context(building.display())?;
    Ok(Slot {
        _locked: slot,
        building,
    })
}

/// How many jobs build at once ([`take_slot`]): one for every two CPUs that this process may run
/// on, and one at least.
// Not one for each: a build runs its recipe's independent stages at once, and a step may itself
// use every CPU; and every build's cache takes from the one volume.
fn slots_count() -> NonZeroUsize {
    let cpus = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    NonZeroUsize::new(cpus / 2).unwrap_or(NonZeroUsize::MIN)
}

/// The id of the newest job whose IR names each image that one names, a run's in its record's
/// `ir.json` in `runs` and a build's in its `out` in `home`: ids sort as the jobs' starts do.
fn last_used(home: &Path, runs: &Path) -> Result<BTreeMap<Image, String>, String> {
    let mut irs = Vec::new();
    for id in ids(&home.join(JOBS))? {
        let ir = runs.join(&id).join("ir.json");
        irs.push((id, ir));
    }
    for id in ids(&home.join(BUILDS))? {
        let out = home.join(BUILDS).join(&id).join("out");
        irs.push((id, out));
    }
    irs.sort();
    let mut last_used = BTreeMap::new();
    for (id, ir) in irs {
        for image in images(&ir)? {
            last_used.insert(image, id.clone());
        }
    }
    Ok(last_used)
}

/// The disk that holds `path`, as `df` says, in bytes: its size, and what an ordinary user may
/// still write there.
fn disk(path: &str) -> Result<Disk, String> {
    // Only `PATH`, so that no locale changes what it prints.
    let mut df = stdx::process::command("df", Environment::Only(&["PATH"]));
    df.args(["--block-size=1", "--output=size,avail", "--", path]);
    let printed = bounded(&mut df, &format!("df {path}"), DF_TIMEOUT)?;
    let printed = String::from_utf8_lossy(&printed);
    // A line of headings, then the sizes.
    let sizes = printed.lines().nth(1).unwrap_or_default();
    let sizes: Vec<Option<u64>> = (sizes.split_whitespace())
        .map(|size| size.parse().ok())
        .collect();
    let [Some(size_bytes), Some(free_bytes)] = sizes[..] else {
        return Err(format!(
            "`df {path}` printed `{}`: expected a line of headings, then the size and what is \
             available, in bytes, as GNU `df` prints them",
            printed.trim().escape_debug()
        ));
    };
    Ok(Disk {
        size_bytes,
        free_bytes,
    })
}

/// The images that the IR at `path` names, none if there is no IR there, as of a job that
/// ended before its run or build did, or a build's `out` that is empty, as a failed build leaves
/// it.
fn images(path: &Path) -> Result<Vec<Image>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) if text.is_empty() => return Ok(Vec::new()),
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let ir: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} isn't JSON: {error}", path.display()))?;
    let roles = ir["roles"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let named = roles.iter().filter_map(|role| role["image"].as_str());
    Ok(named
        .filter_map(|image| Image::try_from(image.to_owned()).ok())
        .collect())
}
