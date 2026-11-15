//! The helper, `pumpkin-modal.py`, through which the provider makes each call on Modal: a Python
//! program on Modal's SDK, which `uv`, found on `PATH`, runs once for each call, with the packages
//! that the lock beside it pins (`uv run --frozen --script`), so that every call runs the same SDK.
//!
//! A call's [`Request`] goes to the helper's standard input as JSON, and its answer comes on its
//! standard output, `{"ok": …}` or `{"error": "…"}`, which is what Modal reported. The helper
//! gets nothing of this process's environment but `HOME` and `PATH`, the profile in
//! `MODAL_PROFILE`, and the settings' configuration file, if they name one, in
//! `MODAL_CONFIG_PATH`, so its token comes from that file, or Modal's own default,
//! `~/.modal.toml`, and no variable of Modal's or of `uv`'s, such as `MODAL_TOKEN_ID` or
//! `UV_INDEX`, steers it to another workspace or to other packages. What `uv` fetches, its cache
//! and the Pythons it downloads, and the temporary files of `uv` and the helper, go in the
//! provider's directory on this machine ([`Helper::new`]). Each call is killed if it hasn't ended
//! by the deadline its caller gives ([`Helper::call`]).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schema::sandbox::RegistrationGrant;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stdx::process::Environment;
use stdx::{Context as _, Deadline};

use crate::Gpu;

/// What the helper gets of this process's environment: what finds `uv`, and Modal's
/// configuration, below the user's home.
const CALL_ENVIRONMENT: Environment = Environment::Only(&["HOME", "PATH"]);

/// How long a call that only asks Modal may run before it is killed.
// One that `uv` runs from its cache starts in about a second, and Modal answers it at once; `uv`
// may first download the lock's packages, and an interpreter, which takes seconds more on a
// sound link, and Modal's SDK retries a lost connection for about a minute, so past two it is
// hung.
pub(crate) const ASK_TIMEOUT: Duration = Duration::from_mins(2);

/// How long an import may run before it is killed.
// Modal pulls the image and builds it within the call: a judge's image of 10 GB took minutes of
// a probe's, and one of 130 GB, with a task's weights in it, about half an hour.
pub(crate) const IMPORT_TIMEOUT: Duration = Duration::from_mins(45);

/// How much of what a failed helper printed on standard error its error keeps: the end, where a
/// Python traceback says what failed.
const REPORTED_BYTES: usize = 4096;

/// A profile of Modal's configuration, `~/.modal.toml`, by its name, which Modal refuses if the
/// configuration lacks it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Profile(String);

/// A file of Modal's configuration, `~/.modal.toml`'s form, by its absolute path.
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "PathBuf")]
pub(crate) struct ConfigFile(PathBuf);

impl TryFrom<PathBuf> for ConfigFile {
    type Error = String;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        // Modal's SDK reads it from wherever the helper runs, so a relative path would name
        // another file there.
        if !path.is_absolute() {
            return Err(format!(
                "`{}` is not an absolute path: name Modal's configuration file by one",
                path.display()
            ));
        }
        Ok(Self(path))
    }
}

/// A call that the helper makes, as its standard input holds it.
#[derive(Serialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Request<'a> {
    /// The workspace that the profile reaches, and the versions of the SDK and of Python.
    Ready,
    /// Imports the image at `reference`, a reference by digest, which Modal pulls with the AWS
    /// role `role_arn` in `region`, and returns its id.
    Import {
        reference: &'a str,
        role_arn: &'a str,
        region: &'a str,
    },
    /// Creates a sandbox of `run`, for `role`, as the helper's own doc says, and returns its id,
    /// its App, and its tunnel's host and port.
    Create {
        run: &'a str,
        role: &'a str,
        /// The image's id in Modal.
        image: &'a str,
        sandboxd: Sandboxd<'a>,
        /// The role's user, `uid:gid`, which `sandboxd` becomes.
        user: String,
        ops: bool,
        model: bool,
        grant: &'a RegistrationGrant,
        cpu_millis: u64,
        memory_mib: u64,
        gpu: Option<&'a Gpu>,
        /// The run's hard end, in whole seconds since the Unix epoch, from which the helper
        /// takes the sandbox's timeout as it creates it, as Modal counts that from then.
        hard_end: u64,
        /// When the sandbox's start is due by, in whole seconds since the Unix epoch, which its
        /// `sandboxd` takes as `--by`, so that one no start claims ends itself.
        by: u64,
        /// The only source, a CIDR, that the sandbox's tunnel takes connections from.
        source: String,
    },
    /// The sandboxes of `run` that haven't finished, each with whether it has ended.
    List { run: &'a str },
    /// Terminates each of `run`'s sandboxes that `ids` name, and returns why each that failed
    /// did.
    Delete { run: &'a str, ids: Vec<&'a str> },
    /// The end of what sandbox `id` of `run` has printed.
    Console { run: &'a str, id: &'a str },
}

/// A published `sandboxd`: the file at `path` on this machine, whose SHA-256 is `sha256`, in
/// hexadecimal digits.
#[derive(Serialize)]
pub(crate) struct Sandboxd<'a> {
    pub(crate) path: &'a str,
    pub(crate) sha256: &'a str,
}

/// What the helper prints on standard output.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Answer<T> {
    Ok(T),
    Error(String),
}

/// The helper at a path, with its lock beside it, run with a profile, of a configuration file
/// if not Modal's default.
#[derive(Debug)]
pub(crate) struct Helper {
    script: PathBuf,
    dir: PathBuf,
    profile: Profile,
    config: Option<ConfigFile>,
}

impl Helper {
    /// The helper at `script`, which `uv` runs with its cache in `dir/uv`, the Pythons it
    /// downloads in `dir/python`, and the temporary directory in `dir/tmp`.
    pub(crate) fn new(
        script: &Path,
        dir: &Path,
        profile: Profile,
        config: Option<ConfigFile>,
    ) -> Self {
        Self {
            script: script.to_owned(),
            dir: dir.to_owned(),
            profile,
            config,
        }
    }

    /// Makes `request`, and returns what the helper answers, as a `T`, or kills the helper at
    /// `by`, if it hasn't ended by then.
    pub(crate) fn call<T: DeserializeOwned>(
        &self,
        request: &Request<'_>,
        by: Deadline,
    ) -> Result<T, String> {
        let Ok(input) = serde_json::to_value(request) else {
            unreachable!("a request holds only strings, which serde_json never fails to write")
        };
        let call = input["call"].as_str().unwrap_or_default();
        // Python's `tempfile` takes another directory, such as `/tmp`, for one that is missing.
        let temp = self.dir.join("tmp");
        fs::create_dir_all(&temp).context(temp.display())?;
        let mut uv = stdx::process::command("uv", CALL_ENVIRONMENT);
        uv.env("UV_CACHE_DIR", self.dir.join("uv"))
            .env("UV_PYTHON_INSTALL_DIR", self.dir.join("python"))
            .env("TMPDIR", temp);
        uv.env("MODAL_PROFILE", &self.profile.0);
        if let Some(ConfigFile(config)) = &self.config {
            uv.env("MODAL_CONFIG_PATH", config);
        }
        uv.args(["run", "--frozen", "--quiet", "--no-config", "--script"]);
        uv.arg(&self.script);
        let output = match stdx::process::fed(&mut uv, input.to_string().as_bytes(), by) {
            Ok(Some(output)) => output,
            Ok(None) => {
                return Err(format!(
                    "the Modal helper's `{call}` hadn't ended by its deadline, so it was killed, \
                     if it had started: expected Modal to answer by then"
                ));
            }
            // A spawn fails so when it finds no `uv`, or one it can't execute.
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::PermissionDenied
                ) =>
            {
                return Err(format!("`uv` didn't run: {error}: install uv on `PATH`"));
            }
            Err(error) => {
                return Err(format!(
                    "running the Modal helper's `{call}` failed: {error}"
                ));
            }
        };
        if !output.status.success() {
            let stderr = &output.stderr;
            let tail = &stderr[stderr.len().saturating_sub(REPORTED_BYTES)..];
            let reported = String::from_utf8_lossy(tail);
            return Err(format!(
                "the Modal helper `{}`'s `{call}` failed with {}: {}: fix what it reports",
                self.script.display(),
                output.status,
                reported.trim()
            ));
        }
        match serde_json::from_slice::<Answer<T>>(&output.stdout) {
            Ok(Answer::Ok(answer)) => Ok(answer),
            // Modal's messages may end with a period, which the error's own punctuation follows.
            Ok(Answer::Error(reported)) => Err(format!(
                "Modal refused the helper's `{call}`: {}: fix what Modal reports",
                reported.trim_end_matches('.')
            )),
            Err(error) => Err(format!(
                "the Modal helper's `{call}` printed what isn't its answer: {error}: expected \
                 `{{\"ok\": …}}`, of the form the call returns, or `{{\"error\": \"…\"}}`"
            )),
        }
    }
}
