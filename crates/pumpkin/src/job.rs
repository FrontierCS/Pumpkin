//! The controller's own side of the controller target, as `pumpkin controller --help` says: a
//! job ([`job`]), which the client starts in a unit of its own, and the controller's boot
//! ([`boot`]).
//!
//! What they read and write lies below the controller user's home: the trees the client sent,
//! `trees/<repo>/<id>/`; each run's job in `jobs/<id>/`, and each build's in `builds/<id>/`,
//! with its configuration, `pumpkin.toml`, which holds no secret, and its unit's standard error
//! and output, `log` and `out`; the one workspace, `.pumpkin/` ([`crate::workspace`]), that
//! every tree's `.pumpkin` links to, where each job's Pumpkin is built, which keeps the runs'
//! records in its runs directory ([`runs`]); and
//! `current-pumpkin`, the `pumpkin` that the last job ran. What the client sent with a job
//! ([`SENT_CONFIG`], [`SENT_MODAL`] and the upstreams' key files) is in the job's directory
//! below `$XDG_RUNTIME_DIR/pumpkin/` ([`jobs`]), a tmpfs that the job's unit removes as it
//! ends, so a job that still runs is one whose directory is there.

use std::net::Ipv4Addr;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use std::{fs, io};

use aws::{Bound, Cli};
use provider::RunTag;
use runtime::Record;
use serde::Deserialize;
use stdx::Context as _;
use stdx::process::Environment;

use crate::controller::bounded;
use crate::{cli, say, workspace};

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

/// The instance metadata service, as its version 2, `IMDSv2`, serves it.
const IMDS: &str = "http://169.254.169.254/latest";

/// How long `curl` may take to answer from the instance metadata service, which is the
/// instance's own.
const IMDS_SECONDS: u64 = 10;

/// Runs the job that `job` asks for: points [`CURRENT`] at this `pumpkin`, writes the job's
/// configuration ([`assemble`]), then runs its command with it, a run to its end; returns the
/// exit status of its command.
///
/// # Errors
///
/// Fails if the job's directory, its configuration or [`CURRENT`] can't be made, or its command
/// fails before it starts.
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
    let kind = match &command {
        cli::JobCommand::Run(_) => JOBS,
        cli::JobCommand::Build(_) => BUILDS,
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
