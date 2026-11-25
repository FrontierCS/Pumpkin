//! The names of a sandbox's containers. Each sandbox holds one container that runs its role's
//! image, named `main`. A runner's sandboxes also hold its companions, each by a [`CompanionName`].

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

/// The most bytes of a DNS label (RFC 1035, section 2.3.4).
const MAX_LABEL_BYTES: usize = 63;

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
        "main" => "names the runner's own container",
        "localhost" => {
            "names each container's own loopback address, so no other container reaches the \
             companion by it"
        }
        _ => return Ok(()),
    };
    Err(format!("`{name}` {why}: give the companion another name"))
}
