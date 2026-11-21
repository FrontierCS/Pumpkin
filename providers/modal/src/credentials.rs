//! What reaches the workspace: a profile of Modal's configuration file, or a token that an SSM
//! `SecureString` parameter holds, which the provider reads each time it reaches the workspace and
//! gives the helper in its environment alone, so that no file, command line or log holds it.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

use aws::{Cli, Region};
use serde::{Deserialize, Serialize};

/// What the settings name to reach the workspace by.
#[derive(Debug)]
pub(crate) enum Credentials {
    Profile(Profile),
    Token(TokenParameter),
}

/// A profile of Modal's configuration file, of `config` if given, else Modal's own default,
/// `~/.modal.toml`.
#[derive(Clone, Debug)]
pub(crate) struct Profile {
    pub(crate) name: ProfileName,
    pub(crate) config: Option<ConfigFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
// The name of a profile in Modal's configuration file, which Modal refuses if the file lacks it.
pub(crate) struct ProfileName(pub(crate) String);

#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "PathBuf")]
pub(crate) struct ConfigFile(pub(crate) PathBuf);

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

/// The SSM parameter, a `SecureString` in `region`, whose value is a Modal token as JSON, by the
/// names that Modal's own configuration gives its two parts: `{"token_id": "ak-…",
/// "token_secret": "as-…"}`.
// Without `deny_unknown_fields`, as `crate::Settings`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TokenParameter {
    region: Region,
    parameter: String,
}

/// A token of Modal's, its ID and its secret.
// Without `deny_unknown_fields`, so that what `modal workspace service-users create --json` prints,
// which names when the token expires beside it, is a token:
// https://github.com/modal-labs/modal-client/blob/e8a24c8e304766d1b3b6905f297e3f43d08e1d93/py/modal/cli/workspace.py#L371-L383
#[derive(Deserialize)]
pub(crate) struct Token {
    pub(crate) token_id: String,
    pub(crate) token_secret: String,
}

// So that no error or log that shows the provider shows its token.
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token { .. }")
    }
}

/// What the helper reaches the workspace with.
#[derive(Debug)]
pub(crate) enum Reaching {
    Profile(Profile),
    Token(Token),
}

impl Reaching {
    /// Sets in `helper`'s environment, the helper's command's, what reaches the workspace.
    pub(crate) fn set_env(&self, helper: &mut Command) {
        match self {
            Self::Profile(Profile { name, config }) => {
                helper.env("MODAL_PROFILE", &name.0);
                if let Some(ConfigFile(config)) = config {
                    helper.env("MODAL_CONFIG_PATH", config);
                }
            }
            // Modal's SDK reads these before any profile, and Modal's guide gives a service user's
            // token so: https://modal.com/docs/guide/service-users
            // https://github.com/modal-labs/modal-client/blob/e8a24c8e304766d1b3b6905f297e3f43d08e1d93/py/modal/config.py#L454-L461
            Self::Token(Token {
                token_id,
                token_secret,
            }) => {
                // An empty configuration, so that no profile of `~/.modal.toml`, such as its
                // `environment`, or its lack of an active one, steers the token's calls.
                helper
                    .env("MODAL_TOKEN_ID", token_id)
                    .env("MODAL_TOKEN_SECRET", token_secret)
                    .env("MODAL_CONFIG_PATH", "/dev/null");
            }
        }
    }
}

impl Credentials {
    /// What reaches the workspace: the profile, or the token that the parameter holds, read now.
    pub(crate) fn read(&self) -> Result<Reaching, String> {
        let parameter = match self {
            Self::Profile(profile) => return Ok(Reaching::Profile(profile.clone())),
            Self::Token(parameter) => parameter,
        };
        let TokenParameter { region, parameter } = parameter;
        let value = Cli::connect(region.clone())?.parameter(parameter)?;
        // The error, which may quote the value, is left out.
        let token = serde_json::from_str(&value).map_err(|_| {
            format!(
                "SSM parameter `{parameter}` holds no Modal token: put one there as \
                 `{{\"token_id\": \"ak-…\", \"token_secret\": \"as-…\"}}`, a SecureString"
            )
        })?;
        Ok(Reaching::Token(token))
    }

    /// What `Provider::ready` reports of them: the profile's name, or the parameter.
    pub(crate) fn report(&self) -> serde_json::Value {
        match self {
            Self::Profile(Profile { name, config: _ }) => serde_json::json!({ "profile": name }),
            Self::Token(parameter) => serde_json::json!({ "token_parameter": parameter }),
        }
    }
}
