//! The jobs of a controller: what `pumpkin controller job`, which the unit of each job runs, does
//! on the controller, and the prune that keeps the controller's volume from filling. That volume
//! is the filesystem of the user's home, which holds the engine's images and build cache too, as
//! `deploy/aws` and `deploy/docker` lay them out.
//!
//! They find the controller's settings through the host file (`crate::host::target`). Everything
//! else they read and write lies below the controller user's home:
//!
//! - the store of the files that clients sent, each named by its SHA-256, as `crate::manifest`
//!   lays it out;
//! - each job's tree, `trees/<repo>/<id>/`, of links into the store;
//! - each run's job in `jobs/<id>/`. It holds the job's configuration, `pumpkin.toml`, which holds
//!   no secret, and its unit's standard error and output, `log` and `out`;
//! - the one workspace, `.pumpkin/`, that every tree's `.pumpkin` links to. It keeps each build's
//!   install, `<sha256>/bin/`. The install's `pumpkin` is the build's `serve`, and each of the
//!   build's jobs runs it. The workspace also keeps the runs' records and the build slots.
//!
//! What the client sent with a job (`SENT_CONFIG`, `SENT_MODAL` and the upstreams' key files) is
//! in the job's directory below `$XDG_RUNTIME_DIR/pumpkin/`, a tmpfs that the job's unit removes
//! as it ends, so a job that still runs is one whose directory is there, as is what images the
//! job holds (`HELD`): what a job holds goes with it, however it ends. This module reads these for
//! the prune, and `retention` decides which of them it removes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, io, thread};

use engine::Engine;
use engine::prune::{Disk, HIGH_PERCENT, Held, REFUSED_PERCENT};
use provider::RunTag;
use runtime::Record;
use schema::ir::{Image, Sha256Digest};
use stdx::Context as _;
use stdx::process::{Bound, Echo, Environment, Failure, Run};

use crate::cli::Exit;
use crate::iam::JOB;
use crate::manifest::{Entry, INCOMING, Kind, MANIFESTS, Manifest};
use crate::workspace::{self, Workspace};
use crate::{cli, config, say, star};

mod boot;
mod installs;
mod member;
mod records;
mod release;
mod settings;
mod source;
mod start;
mod store;

pub(crate) use boot::{BOOT_LOG, BOOT_UNIT, boot, state as boot_state};
pub(crate) use member::{member, run_member, write_member};
pub(crate) use records::mark_copied;
pub(crate) use settings::{Deployment, Taken, deployment, tables as settings_tables, take};
pub(crate) use source::{Source, parameter_value};
pub(crate) use start::start;

pub(crate) const JOBS: &str = "jobs";

pub(crate) const TREES: &str = "trees";

pub(crate) const STORE: &str = "store";

/// The runs directory of the controller's one workspace, relative to the user's home: that of
/// every job's `pumpkin`, as each runs from that workspace.
pub(crate) fn runs() -> String {
    format!("{}/{}", workspace::DIR, workspace::RUNS)
}

/// The start of the name of each job's unit, which its id ends.
pub(crate) const UNIT_PREFIX: &str = "pumpkin-job-";

pub(crate) fn unit_name(id: &RunTag) -> String {
    format!("{UNIT_PREFIX}{}", id.as_str())
}

// The link in a job's directory to the `pumpkin` that runs it.
pub(crate) const OWN: &str = "pumpkin";

pub(crate) const SENT_CONFIG: &str = "config.toml";

pub(crate) const SENT_MODAL: &str = "modal.toml";

// The file of a job's tmpfs directory that names the images and tags it holds, a line each, which
// no prune removes while the job runs.
const HELD: &str = "held";

// The file of a job's tmpfs directory that exists while the job holds a build slot, so may build
// an image that no tag names yet, which no prune then removes.
pub(crate) const BUILDING: &str = "building";

// The file of a job's tmpfs directory that exists while the job waits for a build slot, so that
// `ls` lists it as waiting.
pub(crate) const WAITING: &str = "waiting";

// The bound of `df`, which reads only the volume's counters.
const DF_TIMEOUT: Duration = Duration::from_secs(10);

// The instance metadata service, as its version 2, `IMDSv2`, serves it, which only the instance
// itself reaches.
const IMDS: &str = "http://169.254.169.254/latest";

// How long `curl` may take to answer from the instance metadata service, which is the instance's
// own.
const IMDS_SECONDS: u64 = 10;

/// Runs job `job` in its tree, as its unit does. It assembles the job's configuration and prunes.
/// It builds the task's images under the controller's build slots. Then it runs the task, as the
/// job's `run` says. Returns the run's exit status.
pub(crate) fn job(job: cli::Job) -> Result<Exit, String> {
    let cli::Job { id, tree, run } = job;
    let home = crate::home("run the job as the controller's user")?;
    let working = std::env::current_dir().context("finding the job's working directory failed")?;
    materialize(&home, &id, &tree, &working)?;
    let jobs = jobs()?;
    let sent = jobs.join(id.as_str());
    let dir = home.join(JOBS).join(id.as_str());
    fs::create_dir_all(&dir).context(dir.display())?;
    // Record that the unit ran even if its start receipt could not be written.
    point(&dir.join(OWN), &dir.join(format!(".{OWN}")))?;
    let settings = crate::host::target()?.settings();
    let (assembled, controller) = assemble(&settings, &sent)?;
    let path = dir.join("pumpkin.toml");
    fs::write(&path, assembled).context(path.display())?;
    let Some(config) = path.to_str().map(str::to_owned) else {
        return Err(format!(
            "{} isn't UTF-8, as a run's record keeps it: give the controller's user a home \
             whose path is",
            path.display()
        ));
    };
    // Before the build, so that it has room. The records go first, so that the prune then removes
    // the directory of each run's job whose record went.
    if let Err(error) = records::collect(&home, &jobs) {
        say(format_args!("the prune of the records failed: {error}"));
    }
    if let Err(error) = prune(&home, &path, &jobs) {
        say(format_args!("the prune at the job's start failed: {error}"));
    }
    let task = (&run.entry, &run.args.0);
    build_first(&id, &home, &path, &jobs, task, &controller)?;
    let run = cli::Run {
        config: Some(config),
        controller: Some(controller),
        wait: cli::Waiting(None),
        ..run
    };
    // Only one who follows the run reads the job's log.
    let exit = crate::start(&id, run, crate::Reader::Follower)?;
    // The run's end is the job's, so that the unit lasts as long as the run.
    if exit == Exit::Success {
        let runs = crate::default_runs()?;
        let record = Record::open(&Path::new(&runs).join(id.as_str()))?;
        drop(record.wait(|| {})?);
    }
    Ok(exit)
}

/// Makes the tree named `tree` in `dir`, job `id`'s tree below `home`, of links into the store
/// there, as its manifest names each file: a file to
/// its blob, one that its owner may execute to the blob's copy at 0555, made if need be, and a
/// symbolic link to its blob's text.
///
/// # Errors
///
/// Fails before it writes anything if `dir` isn't `trees/<repo>/<id>` below `home`, as a job run
/// by hand elsewhere would write the task's files there.
// Links, not copies, so that a tree costs no bytes, and in Rust rather than a shell's `ln` for
// each of a tree's thousands of files.
fn materialize(home: &Path, id: &RunTag, tree: &Sha256Digest, dir: &Path) -> Result<(), String> {
    let store = home.join(STORE);
    let path = store.join(MANIFESTS).join(tree.hex());
    let manifest = fs::read(&path).context(path.display())?;
    let manifest = Manifest::parse(&manifest).context(path.display())?;
    let trees = home.join(TREES);
    let above = dir.parent().and_then(Path::parent).map(fs::canonicalize);
    let own = dir.file_name() == Some(OsStr::new(id.as_str()))
        && matches!((fs::canonicalize(&trees), above), (Ok(trees), Some(Ok(above))) if trees == above);
    if !own && !manifest.entries().is_empty() {
        return Err(format!(
            "the job's working directory, {}, isn't its tree, {}/<repository>/{id}, which its \
             start makes: run the job there, as its unit does",
            dir.display(),
            trees.display(),
            id = id.as_str()
        ));
    }
    for entry in manifest.entries() {
        let Entry {
            kind,
            digest,
            size_bytes: _,
            path,
        } = entry;
        let to = dir.join(path);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).context(parent.display())?;
        }
        let blob = store.join(digest.hex());
        let linked = match kind {
            Kind::File => fs::hard_link(&blob, &to),
            Kind::Executable => fs::hard_link(executable(&store, digest)?, &to),
            Kind::Link => {
                let target = fs::read(&blob).context(blob.display())?;
                symlink(OsStr::from_bytes(&target), &to)
            }
        };
        linked.context(to.display())?;
    }
    Ok(())
}

/// The copy at 0555 of the blob `digest` in `store`, `<sha256>.x`, made aside and moved in if it
/// isn't there, as another job may make it at once: a link shares its file's mode, so a file that
/// the tree holds as executable needs a copy of its own.
fn executable(store: &Path, digest: &Sha256Digest) -> Result<PathBuf, String> {
    let copy = store.join(format!("{}.x", digest.hex()));
    if copy.try_exists().context(copy.display())? {
        return Ok(copy);
    }
    let incoming = store.join(INCOMING);
    let aside = tempfile::Builder::new()
        .prefix("executable.")
        .tempfile_in(&incoming);
    let aside = aside.context(incoming.display())?;
    let blob = store.join(digest.hex());
    fs::copy(&blob, aside.path()).context(blob.display())?;
    let made = fs::set_permissions(aside.path(), fs::Permissions::from_mode(0o555));
    made.context(aside.path().display())?;
    aside.persist(&copy).context(copy.display())?;
    Ok(copy)
}

/// The directory in tmpfs that holds each job's own directory of what the client sent with it,
/// `$XDG_RUNTIME_DIR/pumpkin/`, each named by its job's id. Fails if `XDG_RUNTIME_DIR` is unset or
/// empty, as outside a user's systemd unit.
pub(crate) fn jobs() -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join("pumpkin"))
}

pub(crate) fn runtime_dir() -> Result<PathBuf, String> {
    // The same for every job: the user's systemd gives each job's unit the user's runtime
    // directory, as `pam_systemd` gives the session that makes the job's directory there.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty());
    let Some(runtime) = runtime else {
        return Err(
            "XDG_RUNTIME_DIR is unset or empty: run the job in a unit of the user's systemd, as \
             the controller target starts it"
                .to_owned(),
        );
    };
    Ok(runtime.into())
}

/// What the jobs that run hold, of the directories of every job in `jobs`, and their runs'
/// records in `runs`: the images that the IR of each run that has started names, a run having
/// started once its record has its log, which it begins only once it has published every image it
/// runs; each image or tag in a job's `HELD`; and whether one is `BUILDING`. A job that runs is
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

/// The directory of every job's directory in tmpfs, `jobs`, opened to be locked: a prune locks it
/// while it reads what the jobs hold and removes the images they don't (`collect`), and a job
/// shares the lock while it adds to what it holds (`Admission`), so that no image a job has held
/// before it looks it up is removed while the job runs.
fn open_jobs(jobs: &Path) -> Result<File, String> {
    File::open(jobs).context(jobs.display())
}

// The file in `jobs` whose lock a prune of the images holds to its end (`collect`), so that prunes
// run one at a time. It isn't the jobs' lock, which a cancel, a start and a build's hold wait for,
// as the build cache's prune can take minutes.
const PRUNING: &str = ".pruning";

/// Points `link` at this `pumpkin`, replacing what it pointed at whole, by way of `staged`, a path
/// beside it that no other job uses.
fn point(link: &Path, staged: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().context("finding this `pumpkin`'s executable failed")?;
    symlink(&exe, staged).context(staged.display())?;
    fs::rename(staged, link).context(link.display())
}

/// A job's configuration, as TOML. It holds the controller's settings, read from `settings`, as
/// [`configuration`] makes them one, with the gateway at the controller's private IPv4 address if
/// they name an EC2 provider. It holds each Modal provider as the client sent it, with
/// `dials_from` at the controller's public IPv4 address, and `config`, the client's Modal file in
/// `sent`, if any. It holds the client's `[upstreams]`, each `key_file` its file in `sent`.
///
/// Fails if the settings drain.
fn assemble(settings: &Source, sent: &Path) -> Result<(String, crate::controller::Name), String> {
    let value = settings.read(&JOB.settings)?;
    let named = format!("the controller's settings, {settings},");
    let mut tables = settings::tables(&value, &named)?;
    let Taken {
        version: envelope,
        draining,
    } = settings::take(&mut tables, &named)?;
    if draining {
        return Err(settings::drained(&named));
    }
    // Only an EC2 instance has its metadata service, so a controller elsewhere, whose settings
    // name no EC2 provider, and whose client sends no Modal provider, asks for no address.
    let private = names_kind(&tables, "ec2")
        .then(|| address("local-ipv4"))
        .transpose()?;
    let config = sent.join(SENT_CONFIG);
    let text = match fs::read_to_string(&config) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("{}: {error}", config.display())),
    };
    let config::Sent {
        controller,
        mut upstreams,
        modal,
    } = toml::from_str(&text).map_err(|error| {
        let error = error.to_string();
        format!(
            "what the run sent, {SENT_CONFIG}, is refused: {}",
            error.trim_end()
        )
    })?;
    let mut modal = modal
        .map(toml::Table::try_from)
        .transpose()
        .map_err(|error| written(&error))?;
    let modal_config = sent.join(SENT_MODAL);
    let modal_config = modal_config
        .try_exists()
        .context(modal_config.display())?
        .then(|| modal_config.to_string_lossy().into_owned());
    if let Some(modal) = &mut modal {
        let public = address("public-ipv4")?;
        modal.insert("dials_from".to_owned(), public.to_string().into());
        if let Some(config) = modal_config {
            modal.insert("config".to_owned(), config.into());
        }
    }
    upstreams.keys_in(sent);
    let upstreams = toml::Value::try_from(upstreams).map_err(|error| written(&error))?;
    let assembled = configuration(tables, &named, envelope, private, modal.as_ref(), upstreams)?;
    Ok((assembled, controller))
}

/// A job's configuration, as TOML. It holds `tables`, the controller's settings named as `named`,
/// of envelope `envelope`, as [`settings::take`] takes them. `[gateway] private` is set to
/// `private`, port 0, if given. Each Modal provider is set as `modal` says ([`take_modal`]).
/// `upstreams` are the client's.
///
/// Fails if the settings set `gateway` or `upstreams`, which each job sets. Fails too if the result
/// isn't a configuration.
pub(crate) fn configuration(
    mut tables: toml::Table,
    named: &str,
    envelope: settings::Version,
    private: Option<Ipv4Addr>,
    modal: Option<&toml::Table>,
    upstreams: toml::Value,
) -> Result<String, String> {
    for key in ["gateway", "upstreams"] {
        if tables.contains_key(key) {
            return Err(format!(
                "{named} hold `[{key}]`, which each job sets: remove it from them"
            ));
        }
    }
    if let Some(private) = private {
        let gateway =
            toml::Table::from_iter([("private".to_owned(), format!("{private}:0").into())]);
        tables.insert("gateway".to_owned(), gateway.into());
    }
    take_modal(&mut tables, modal, named)?;
    tables.insert("upstreams".to_owned(), upstreams);
    settings::remove_unknown(&mut tables, named, envelope)?;
    toml::to_string(&tables).map_err(|error| written(&error))
}

fn written(error: &toml::ser::Error) -> String {
    format!("writing the job's configuration: {error}")
}

/// Makes each Modal provider of the controller's settings, `tables`, the one that the client sent,
/// `sent`, with the settings' `images`, or removes it if the client sent none. Fails, naming the
/// settings as `named`, if the client sent one, but they name none.
fn take_modal(
    tables: &mut toml::Table,
    sent: Option<&toml::Table>,
    named: &str,
) -> Result<(), String> {
    // What a Modal workspace offers is the client's to say, so that the person changes it with no
    // apply; the controller's settings give the provider only what the module made.
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
            "{named} name no Modal provider, whose `images` the client's controller's `modal` \
             needs: list the profile's workspace in deploy/aws's `modal_workspaces`, and apply it"
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

/// Whether `tables`, a controller's settings, name a provider of kind `kind`.
pub(crate) fn names_kind(tables: &toml::Table, kind: &str) -> bool {
    let providers = tables.get("providers").and_then(toml::Value::as_table);
    (providers.into_iter().flatten()).any(|(_, kinds)| kinds.get(kind).is_some())
}

/// This instance's IPv4 address `item`, `local-ipv4` or `public-ipv4`, as its metadata service
/// serves it under an `IMDSv2` token, through `curl`.
fn address(item: &str) -> Result<Ipv4Addr, String> {
    // Long enough for the read that follows.
    let ttl = "X-aws-ec2-metadata-token-ttl-seconds: 60";
    let token = curl(&[
        "--request",
        "PUT",
        "--header",
        ttl,
        &format!("{IMDS}/api/token"),
    ])?;
    let token = format!("X-aws-ec2-metadata-token: {token}");
    let address = curl(&["--header", &token, &format!("{IMDS}/meta-data/{item}")])?;
    address.parse().map_err(|_| {
        format!(
            "the instance metadata's `{item}` is `{}`, which is no IPv4 address: expected the \
             controller's, as its subnet gives it",
            address.escape_debug()
        )
    })
}

fn curl(args: &[&str]) -> Result<String, String> {
    // Only `PATH`, so that no proxy variable sends it elsewhere.
    let mut curl = stdx::process::command("curl", Environment::Only(&["PATH"]));
    curl.args([
        "--disable",
        "--silent",
        "--show-error",
        "--fail",
        "--max-time",
        &IMDS_SECONDS.to_string(),
    ]);
    curl.args(args);
    let shown = format!("curl {}", args.last().copied().unwrap_or_default());
    let timeout = Duration::from_secs(2 * IMDS_SECONDS);
    let how = Run::new(Bound::Within(timeout), Echo::Silent).named(&shown);
    let printed = stdx::process::run(&mut curl, how).map_err(|failure| match failure {
        Failure::Unstarted { .. } | Failure::Unwatched { .. } => failure.to_string(),
        Failure::Unended { .. } | Failure::Exited { .. } => format!(
            "{}: expected the instance metadata service to answer, as on an EC2 instance",
            failure.unadvised()
        ),
    })?;
    Ok(String::from_utf8_lossy(&printed.stdout).trim().to_owned())
}

/// The ids below `dir`, each a job's or a tree's, by name, which sort as their starts do: the names
/// that begin with a digit, as an id does; none if `dir` isn't there.
pub(crate) fn ids(dir: &Path) -> Result<Vec<String>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let name = entry.context(dir.display())?.file_name();
        // A repository's directory of trees may hold other names beside its trees' ids.
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

/// Prunes what the jobs in `home` keep on the controller's volume. A job that runs has its
/// directory in `jobs`.
///
/// It removes, in this order, the trees, the installs (`installs::collect`), the store's files
/// (`store::collect`) and the jobs' directories that `retention` picks. Then, once the volume is
/// full, it removes the images and the build cache of the engine that the configuration at `config`
/// names (`collect`). It spares what the jobs hold.
fn prune(home: &Path, config: &Path, jobs: &Path) -> Result<(), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let now = SystemTime::now();
    for repo in fs::read_dir(home.join(TREES)).into_iter().flatten() {
        let repo = repo.context("listing the trees failed")?.path();
        for id in ids(&repo)? {
            if retention::jobs::expired(started(&id), now) {
                let tree = repo.join(id);
                fs::remove_dir_all(&tree).context(tree.display())?;
            }
        }
    }
    if let Err(error) = installs::collect(home, jobs) {
        say(format_args!("pruning the installs failed: {error}"));
    }
    store::collect(&home.join(STORE))?;
    let mut observed = Vec::new();
    // Only an expired job's directory may go, so only those are read.
    let expired = |id: &String| retention::jobs::expired(started(id), now);
    for id in ids(&home.join(JOBS))?.into_iter().filter(expired) {
        let record = runs.join(&id);
        let recorded = record.try_exists().context(record.display())?;
        observed.push(retention::jobs::Job {
            started: started(&id),
            id,
            recorded,
        });
    }
    for job in retention::jobs::removed(&observed, now) {
        let dir = home.join(JOBS).join(&job.id);
        fs::remove_dir_all(&dir).context(dir.display())?;
    }
    // The engine is reached only once there may be something to remove.
    if !disk(home)?.full() {
        return Ok(());
    }
    collect(home, &engine(config)?, jobs)
}

/// When job `id` started, or `None` if its id says no time.
pub(crate) fn started(id: &str) -> Option<SystemTime> {
    runtime::minted(id).and_then(|since| SystemTime::UNIX_EPOCH.checked_add(since))
}

fn engine(config: &Path) -> Result<Engine, String> {
    crate::configured_engine(config, crate::Reader::Follower)
}

/// Removes the images of `engine` that `engine::prune::evicted` picks, each last used by the newest
/// job whose IR names it (`last_used`), sparing what the jobs that run hold (`held`), while the
/// jobs' directory is locked (`open_jobs`). Then it prunes the build cache down to the same mark.
/// It holds `PRUNING` throughout, and prunes only if the volume is full once it holds both locks.
/// So a prune that waited for another removes only what that one left.
fn collect(home: &Path, engine: &Engine, jobs: &Path) -> Result<(), String> {
    let runs = PathBuf::from(crate::default_runs()?);
    let last_used = last_used(home, &runs)?;
    let path = jobs.join(PRUNING);
    let pruning = (OpenOptions::new().write(true).create(true).truncate(false))
        .mode(0o600)
        .open(&path)
        .context(path.display())?;
    pruning.lock().context(path.display())?;
    // Before the jobs' lock, which the jobs' builds wait for: an image made since is in no list,
    // and one held since is in what is read under it.
    let images = engine.images()?;
    let locked = open_jobs(jobs)?;
    locked.lock().context(jobs_named(jobs))?;
    let disk = disk(home)?;
    if !disk.full() {
        return Ok(());
    }
    let held = held(jobs, &runs)?;
    let last_used = |image: &Image| last_used.get(image).cloned();
    let picked = engine::prune::evicted(disk, images, last_used, &held);
    // Picked by tags listed before the lock: an image may have been tagged since, as a build that
    // ended meanwhile tags what it built, so each is looked at again, now that nothing that a job
    // holds can change, and the pick is made again of what they are now.
    let evicted = engine::prune::evicted(disk, engine.inspected(&picked)?, last_used, &held);
    engine.remove_images(&evicted)?;
    drop(locked);
    say(format_args!(
        "the volume is more than {HIGH_PERCENT}% used: the prune removed {}, least recently used \
         first",
        crate::counted(evicted.len(), "image")
    ));
    engine.prune_build_cache(disk.low_free_bytes())
}

fn jobs_named(jobs: &Path) -> String {
    format!("locking {} failed", jobs.display())
}

/// Builds the images of the task whose entry and args `task` gives, for job `id`. It builds on the
/// engine of the configuration at `config`. The job's `Admission` admits each build. The job's
/// command then finds each image by its tag. If the job built one, it lets go of the build slot
/// that the admission took, and prunes. The job holds what it holds until it ends. Gives the
/// task's problems, or each build that failed, said. Fails before the first build, naming the
/// controller by the client's name for it, `controller`, if the task's roles never fit the
/// configuration's providers.
fn build_first(
    id: &RunTag,
    home: &Path,
    config: &Path,
    jobs: &Path,
    task: (&star::Entry, &lang::Args),
    controller: &crate::controller::Name,
) -> Result<(), String> {
    let (entry, args) = task;
    let mut admission = Admission {
        id,
        controller,
        home,
        jobs,
        slots: Workspace::find()?.slots(),
        slot: None,
    };
    let built = star::checked(entry, args, |task| {
        let engine = engine(config)?;
        // Before a build slot is waited for and anything is built.
        let entry = Path::new(entry.path());
        crate::run::never_fits_controller(task.workflow(), entry, config, &engine, controller)?;
        let tags = match task.build(&engine, &mut admission) {
            Ok((_, tags, _)) => tags,
            Err(problems) => return Ok(Err(problems)),
        };
        // By their IDs too, until the job ends, as its run publishes them, however long it waits
        // to: their tags are held already, but another build of the same inputs may move a tag to
        // an image of its own.
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

/// What admits the builds of job `id` (`star::Admission`).
///
/// It holds each image that the builds look up by its tag. The tag goes in the job's `HELD`, in
/// its directory in `jobs`, under the shared lock of `jobs` (`open_jobs`). Once the builds have
/// ended, it holds each image they found or built by its ID too. It takes one of the controller's
/// build slots (`take_slot`) as the first build starts, and keeps the slot until its holder lets
/// go. It prunes (`collect`) as each build starts, and as each ends while others run. Then it
/// refuses to start a build while more than `REFUSED_PERCENT` of the volume is used. The refusal
/// names the records that their members haven't copied, which no prune removes.
struct Admission<'j> {
    id: &'j RunTag,
    /// The client's name for the controller, which the commands that a refusal names name.
    controller: &'j crate::controller::Name,
    home: &'j Path,
    jobs: &'j Path,
    slots: PathBuf,
    slot: Option<Slot>,
}

impl Admission<'_> {
    fn sent(&self) -> PathBuf {
        self.jobs.join(self.id.as_str())
    }

    /// Prunes the images and the build cache of `engine`, once the volume is full, saying why the
    /// prune failed, if it did, as the build goes on.
    fn prune(&self, engine: &Engine) {
        let pruned = disk(self.home).and_then(|disk| {
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

    /// Holds each of `named`, an image's tag or ID, in the job's `HELD`, sharing the jobs'
    /// directory's lock, so that no prune picks it as it holds the lock.
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
        let disk = disk(self.home)?;
        if disk.refused() {
            let used_bytes = disk.size_bytes.saturating_sub(disk.free_bytes);
            let used = (used_bytes.saturating_mul(100))
                .checked_div(disk.size_bytes)
                .unwrap_or(100);
            let uncopied = records::uncopied(self.home, self.jobs, self.controller);
            return Err(format!(
                "the controller's volume is {used}% used, past the {REFUSED_PERCENT}% at which a \
                 build is refused, though the prune has removed what no job holds: free space \
                 there, or let the jobs that run end, then run the task again{uncopied}"
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

/// A build slot that a job holds, until it is dropped, with the job's `BUILDING`, which goes with
/// it.
struct Slot {
    _locked: File,
    building: PathBuf,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // One left by a failure here goes with the job's directory as the job ends; until then, it
        // only spares untagged images, which no job may need.
        let _ = fs::remove_file(&self.building);
    }
}

/// Takes one of the controller's build slots, files of `dir`, as many as `slots_count` gives, as
/// `stdx::slots::take` says, named by job `id`: while each is held, it says which jobs hold them,
/// with `WAITING` beside `sent`, then how long it waited; then it marks `BUILDING` beside `sent`.
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

/// How many jobs build at once (`take_slot`): one for every two CPUs that this process may run on,
/// and one at least.
// Not one for each: a build runs its recipe's independent stages at once, and a step may itself
// use every CPU; and every build's cache takes from the one volume.
fn slots_count() -> NonZeroUsize {
    let cpus = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    NonZeroUsize::new(cpus / 2).unwrap_or(NonZeroUsize::MIN)
}

/// The id of the newest job whose IR names each image that an IR names. A job's IR is its run's
/// record's `ir.json` in `runs`. Ids sort as the jobs' starts do.
fn last_used(home: &Path, runs: &Path) -> Result<BTreeMap<Image, String>, String> {
    let mut last_used = BTreeMap::new();
    for id in ids(&home.join(JOBS))? {
        let ir = runs.join(&id).join("ir.json");
        for image in images(&ir)? {
            last_used.insert(image, id.clone());
        }
    }
    Ok(last_used)
}

/// The disk that holds `path`, as `df` says, in bytes: its size, and what an ordinary user may
/// still write there.
pub(crate) fn disk(path: &Path) -> Result<Disk, String> {
    // Only `PATH`, so that no locale changes what it prints.
    let mut df = stdx::process::command("df", Environment::Only(&["PATH"]));
    df.args(["--block-size=1", "--output=size,avail", "--"])
        .arg(path);
    let how = Run::new(Bound::Within(DF_TIMEOUT), Echo::Silent);
    let printed = stdx::process::run(&mut df, how).map_err(|failure| failure.to_string())?;
    let printed = String::from_utf8_lossy(&printed.stdout);
    // A line of headings, then the sizes.
    let sizes = printed.lines().nth(1).unwrap_or_default();
    let sizes: Vec<Option<u64>> = (sizes.split_whitespace())
        .map(|size| size.parse().ok())
        .collect();
    let [Some(size_bytes), Some(free_bytes)] = sizes[..] else {
        return Err(format!(
            "`df` printed `{}`: expected a line of headings, then the size and what is \
             available, in bytes, as GNU `df` prints them",
            printed.trim().escape_debug()
        ));
    };
    Ok(Disk {
        size_bytes,
        free_bytes,
    })
}

/// The images that the IR at `path` names. It names none if there is no IR there, as for a job
/// that ended before its run started.
fn images(path: &Path) -> Result<Vec<Image>, String> {
    let text = match fs::read_to_string(path) {
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
