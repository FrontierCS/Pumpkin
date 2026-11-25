//! The names of a sandbox's containers. Each sandbox holds one container that runs its role's
//! image, named `main`. A runner's sandboxes also hold its companions, each by a [`CompanionName`].
//! A [`Container`] names either.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::values::checked_string;

checked_string!(
    /// The name of a runner's companion, and its host name within the sandbox.
    ///
    /// It is a DNS label of 1 to 63 characters from a-z, 0-9 and `-`. It starts with a letter and
    /// ends with a letter or a digit. It is neither `main`, the name of the runner's own
    /// container, nor `localhost`.
    CompanionName,
    check_companion_name
);

/// The name of the container that runs a sandbox's role's image.
const MAIN: &str = "main";

/// The most bytes of a DNS label (RFC 1035, section 2.3.4).
pub(crate) const MAX_LABEL_BYTES: usize = 63;

/// One of a sandbox's containers: `main`, or one of a runner's companions.
///
/// Its JSON form is a string: `main`, or the companion's name, which is never `main`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Container {
    #[default]
    Main,
    Companion(CompanionName),
}

impl TryFrom<String> for Container {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if name == MAIN {
            Ok(Self::Main)
        } else {
            CompanionName::try_from(name).map(Self::Companion)
        }
    }
}

impl From<Container> for String {
    fn from(container: Container) -> Self {
        match container {
            Container::Main => MAIN.to_owned(),
            Container::Companion(name) => name.into(),
        }
    }
}

impl fmt::Display for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Main => f.write_str(MAIN),
            Self::Companion(name) => name.fmt(f),
        }
    }
}

fn check_companion_name(name: &str) -> Result<(), String> {
    // A letter first. RFC 1123 allows a host name's leading digit only because its highest label
    // is alphabetic, so it never reads as a dotted address. A lone label has no such label:
    // https://www.rfc-editor.org/rfc/rfc1123#section-2.1
    // Kubernetes holds the names of Services, which other pods resolve, to the same rule:
    // https://github.com/kubernetes/kubernetes/blob/af46c47ce925f4c4ad5cc8d1fca46c7b77d13b38/pkg/apis/core/validation/validation.go#L226
    let label = match name.as_bytes() {
        [first, rest @ ..] => {
            let inner =
                |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-';
            first.is_ascii_lowercase()
                && rest.iter().all(inner)
                && !name.ends_with('-')
                && name.len() <= MAX_LABEL_BYTES
        }
        [] => false,
    };
    if !label {
        return Err(format!(
            "`{}` is not a companion's name: use 1 to 63 characters from a-z, 0-9 and `-`, \
             starting with a letter and ending with a letter or a digit",
            name.escape_debug()
        ));
    }
    let why = match name {
        MAIN => "names the runner's own container",
        "localhost" => {
            "names each container's own loopback address, so no other container reaches the \
             companion by it"
        }
        _ => return Ok(()),
    };
    Err(format!("`{name}` {why}: give the companion another name"))
}
