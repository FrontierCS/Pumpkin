//! The helper, `pumpkin-modal.py`, through which the provider makes each call on Modal: a Python
//! program on Modal's SDK, which `uv` runs once for each call, with the packages that the lock
//! beside it pins (`uv run --frozen --script`), so that every call runs the same SDK.
//!
//! A call's [`Request`] goes to the helper's standard input as JSON, and its answer comes on its
//! standard output, `{"ok": …}` or `{"error": "…"}`, which is what Modal reported. Each call is
//! killed if it hasn't ended by the deadline its caller gives.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schema::sandbox::RegistrationGrant;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use stdx::process::{Bound, Echo, Environment, Failure, Run};
use stdx::{Context as _, Deadline};

use crate::Gpu;

// `HOME` is where Modal's own configuration lies, and `PATH` where `uv` is. No variable of Modal's
// or of `uv`'s, such as `MODAL_TOKEN_ID` or `UV_INDEX`, steers the helper to another workspace or
// to other packages.
const CALL_ENVIRONMENT: Environment = Environment::Only(&["HOME", "PATH"]);

// A call that `uv` runs from its cache starts in about a second, and Modal answers it at once;
// `uv` may first download the lock's packages and an interpreter, which takes seconds more, and
// Modal's SDK retries a lost connection for about a minute, so past two a call is hung:
// https://github.com/modal-labs/modal-client/blob/dbb24aba92e2c6c4fa8a123234813cfa48e09b0b/py/modal/_utils/grpc_utils.py#L427-L428
pub(crate) const ASK_TIMEOUT: Duration = Duration::from_mins(2);

// Modal pulls the image and builds it within the call, which takes minutes for a large image and
// about half an hour for one that holds a task's weights.
pub(crate) const IMPORT_TIMEOUT: Duration = Duration::from_mins(45);

#[derive(Clone, Debug, Deserialize, Serialize)]
// The name of a profile in Modal's configuration file, which Modal refuses if the file lacks it.
pub(crate) struct Profile(String);

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

/// A call of the helper, which it reads on its standard input as JSON, `call` naming it.
#[derive(Serialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub(crate) enum Request<'a> {
    /// Reaches the workspace that the profile names.
    Ready,
    /// Imports the image at `reference`, a reference by digest, which Modal pulls with the AWS
    /// role `role_arn` in `region`, and answers its id.
    Import {
        reference: &'a str,
        role_arn: &'a str,
        region: &'a str,
    },
    /// Creates a sandbox of `run`, for `role`, and answers its id and its tunnel's host and port.
    Create {
        run: &'a str,
        role: &'a str,
        image: &'a str,
        sandboxd: Sandboxd<'a>,
        user: String,
        ops: bool,
        model: bool,
        grant: &'a RegistrationGrant,
        cpu_millis: u64,
        memory_mib: u64,
        gpu: Option<&'a Gpu>,
        /// The sandbox's hard end, in whole seconds since the Unix epoch, from which the helper
        /// takes the sandbox's timeout as it creates it, as Modal counts that from then.
        hard_end: u64,
        /// When the sandbox's start is due by, in whole seconds since the Unix epoch, which its
        /// `sandboxd` takes as `--by`, so that one no start claims ends itself.
        by: u64,
        /// The only source, a CIDR, that the sandbox's tunnels take connections from.
        source: String,
        /// The sandbox's companions, each a sidecar, behind its relay, which the helper makes
        /// only for a sandbox that holds some, and whose tunnel it answers too.
        companions: Vec<Companion<'a>>,
        /// The arguments that have the sandbox's own `sandboxd` take the connections of the links
        /// from it.
        links: Vec<String>,
    },
    /// The sandboxes of `run` that haven't finished, each with whether it has ended.
    List { run: &'a str },
    /// Terminates each of `run`'s sandboxes that `ids` name, and answers why each that failed did.
    Delete { run: &'a str, ids: Vec<&'a str> },
    /// The end of what sandbox `id` of `run` has printed.
    Console { run: &'a str, id: &'a str },
}

/// A companion of a sandbox to create: its name, its image's id, its user as `uid:gid`, what its
/// `sandboxd` listens with, as the digits that its environment holds, and the arguments that have
/// it take the connections of the links from it.
#[derive(Serialize)]
pub(crate) struct Companion<'a> {
    pub(crate) links: Vec<String>,
    pub(crate) name: &'a str,
    pub(crate) image: &'a str,
    pub(crate) user: String,
    pub(crate) grant: &'a RegistrationGrant,
    pub(crate) certificate: String,
    pub(crate) key: String,
}

#[derive(Serialize)]
pub(crate) struct Sandboxd<'a> {
    pub(crate) path: &'a str,
    pub(crate) sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Answer<T> {
    Ok(T),
    Error(String),
}

/// The helper at `script`, with its lock beside it, run with a profile, of a configuration file if
/// not Modal's default. What `uv` fetches, its cache and the Pythons it downloads, and the
/// temporary files of `uv` and the helper, go under `dir`.
#[derive(Debug)]
pub(crate) struct Helper {
    script: PathBuf,
    dir: PathBuf,
    profile: Profile,
    config: Option<ConfigFile>,
}

impl Helper {
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

    /// Makes `request` and returns the helper's answer, a `T`, or kills the helper if it hasn't
    /// ended by `by`.
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
        // Runs the lock file beside the script as it is, and reads no `uv` configuration.
        uv.args(["run", "--frozen", "--quiet", "--no-config", "--script"]);
        uv.arg(&self.script);
        let input = input.to_string();
        let how = Run::new(Bound::By(by), Echo::Silent).input(input.as_bytes());
        let output = stdx::process::run(&mut uv, how).map_err(|failure| {
            let failed = match failure {
                Failure::Unended { started: true, .. } => {
                    format!("{}: expected Modal to answer by then", failure.unadvised())
                }
                // Only the caller, which chose the deadline, can say what to do.
                Failure::Unended { started: false, .. } => failure.unadvised(),
                Failure::Unstarted { .. } | Failure::Unwatched { .. } | Failure::Exited { .. } => {
                    failure.to_string()
                }
            };
            format!("the Modal helper's `{call}`: {failed}")
        })?;
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
