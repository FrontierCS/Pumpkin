//! Where the controller's settings and its members' list are read: an SSM parameter, or a file of
//! the host's ([`Source`]).

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::PathBuf;

use aws::iam::Call;
use aws::{Bound, Cli, ParameterArn};
use serde::Deserialize;

/// The most bytes of a file that the controller reads: SSM's bound of an advanced parameter's
/// value, which holds the same document on EC2.
// https://docs.aws.amazon.com/systems-manager/latest/userguide/parameter-store-advanced-parameters.html
const FILE_BYTES: u64 = 8 * 1024;

/// A document that the controller reads, each time anew.
#[derive(Clone, Deserialize)]
#[serde(try_from = "String")]
pub(crate) enum Source {
    Parameter(ParameterArn),
    /// An absolute path.
    File(PathBuf),
}

impl TryFrom<String> for Source {
    type Error = String;

    fn try_from(source: String) -> Result<Self, Self::Error> {
        if source.starts_with('/') {
            return Ok(Self::File(source.into()));
        }
        if source.starts_with("arn:") {
            return ParameterArn::try_from(source).map(Self::Parameter);
        }
        Err(format!(
            "`{}` is neither an SSM parameter's ARN nor an absolute path: name the parameter, as \
             deploy/aws writes it, or the file",
            source.escape_debug()
        ))
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parameter(arn) => write!(f, "SSM parameter `{arn}`"),
            Self::File(path) => write!(f, "file {}", path.display()),
        }
    }
}

impl Source {
    /// The document's text. A parameter is read as `call` declares.
    pub(crate) fn read(&self, call: &'static Call) -> Result<String, String> {
        match self {
            Self::Parameter(arn) => {
                let cli = Cli::connect(arn.region().clone())?;
                parameter_value(&cli, call, arn.as_str())
            }
            Self::File(path) => {
                let read = |error| format!("{}: {error}", path.display());
                let mut text = String::new();
                let file = File::open(path).map_err(read)?;
                file.take(FILE_BYTES + 1)
                    .read_to_string(&mut text)
                    .map_err(read)?;
                if text.len() as u64 > FILE_BYTES {
                    return Err(format!(
                        "{} holds more than {FILE_BYTES} bytes: keep it within them, as SSM \
                         keeps a parameter's",
                        path.display()
                    ));
                }
                Ok(text)
            }
        }
    }
}

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

/// The value of SSM parameter `name`. `cli` reads it as `call` declares.
pub(crate) fn parameter_value(
    cli: &Cli,
    call: &'static Call,
    name: &str,
) -> Result<String, String> {
    let got: GotParameter = cli.json(call, &["--name", name], Bound::Call)?;
    Ok(got.parameter.value)
}
