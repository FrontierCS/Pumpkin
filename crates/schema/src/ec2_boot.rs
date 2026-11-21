//! What the EC2 provider and `ec2-boot`, the boot of each of its sandboxes' instances, share.
//!
//! The provider's user data ([`user_data`]) carries a [`Start`] in a part of its own, beside the
//! cloud-config that runs `ec2-boot`, which reads the start back ([`start_in`]) from cloud-init's
//! copy of the user data ([`USER_DATA_FILE`]). The start is in no cloud-config: cloud-init logs
//! the cloud-config it merges, which AL2023's `yum-variables` module does whole. Of a part whose
//! type it has no handler for, cloud-init logs the first line alone, at most 24 characters of it,
//! which holds no value:
//! <https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/cloudinit/handlers/__init__.py#L180-L252>.

use std::net::SocketAddrV4;

use serde::{Deserialize, Serialize};

/// Where `sandboxd` is in the image of it that a provider whose sandboxes pull from a registry
/// pushes.
pub const SANDBOXD_IN_IMAGE: &str = "/sandboxd";

/// Where `ec2-boot` is in that image.
pub const BOOT_IN_IMAGE: &str = "/ec2-boot";

/// The directory, in memory and root's alone, that `ec2-boot` makes on an instance's first boot,
/// and in which it keeps the file that gives `sandboxd` its attestation ([`Identity`]).
pub const ATTESTATION_DIR: &str = "/run/pumpkin";

/// Where cloud-init keeps the instance's user data as it got it, root's alone:
/// <https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/cloudinit/stages.py#L483-L487>.
pub const USER_DATA_FILE: &str = "/var/lib/cloud/instance/user-data.txt";

/// The start of each line that an instance's boot writes to its serial console.
pub const CONSOLE_PREFIX: &str = "pumpkin-ec2:";

// The user data's parts' boundary, which neither part may hold.
const BOUNDARY: &str = "pumpkin-ec2-part";

// A type that cloud-init has no handler for, so it leaves the part to `ec2-boot`.
const START_TYPE: &str = "application/x-pumpkin-ec2-start";

// The first line of the start's part, the only one that cloud-init logs.
const START_LINE: &str = "pumpkin-ec2 start";

/// The sandbox that `ec2-boot start` starts on its instance: the role's container, whose first
/// process is `sandboxd`, dialing [`gateway`](Self::gateway).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    /// The run's hard end, in whole seconds since the Unix epoch, rounded down. Once the
    /// container has started, the instance starts powering off then, and the boot's wait for the
    /// container ends by then.
    pub hard_end_unix_seconds: u64,
    /// The create's deadline, in whole seconds since the Unix epoch, rounded down, at most the
    /// hard end. From the boot unit's scheduling of it, its first step but a console line, until
    /// the container has started, the instance starts powering off then, whatever hangs, and each
    /// command of the boot's is killed by then. Before that, as while cloud-init runs, only the
    /// provider's timer bounds the instance.
    pub pulls_by_unix_seconds: u64,
    /// The role's image, by digest, which the boot pulls.
    pub role_image: String,
    /// The image of `sandboxd`, by digest, which the user data has pulled.
    pub sandboxd_image: String,
    /// The options of `docker run` that confine the container, as `engine::confinement` gives
    /// them.
    pub confinement: Vec<String>,
    /// The gateway's address, the only one the container reaches.
    pub gateway: SocketAddrV4,
    /// Whether the role makes ops (`provider::Create::ops`).
    pub ops: bool,
    /// Whether `sandboxd` serves the model endpoint
    /// (`provider::Create::model`).
    pub model: bool,
}

/// What the attestation of an EC2 sandbox holds, as JSON: its instance's identity document.
///
/// Its document and the document's signature by AWS are each as the instance's metadata serves
/// them, by which the provider knows the instance:
/// <https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/instance-identity-documents.html>.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    /// The document, JSON, byte for byte as it is signed.
    pub document: String,
    /// The base64 of the document's RSA signature with SHA-256, in lines.
    pub signature: String,
}

/// The user data of a MIME multipart message, which cloud-init reads: `cloud_config`, a
/// cloud-config, and `start`, in a part of its own that [`start_in`] reads.
///
/// # Errors
///
/// A part holds the parts' boundary.
pub fn user_data(cloud_config: &str, start: &Start) -> Result<String, String> {
    let Ok(start) = serde_json::to_string(start) else {
        unreachable!(
            "a start can't be written as JSON, though it holds only strings, numbers, booleans \
             and an address"
        )
    };
    let delimiter = format!("--{BOUNDARY}");
    for (part, text) in [("cloud-config", cloud_config), ("start", &start)] {
        if text.contains(&delimiter) {
            // The part alone, never its text, which may be long.
            return Err(format!(
                "the user data's {part} holds `{delimiter}`, the parts' boundary: expected none"
            ));
        }
    }
    Ok(format!(
        "{}{cloud_config}\n{}{start}\n--{BOUNDARY}--\n",
        header(),
        start_part()
    ))
}

/// The start that [`user_data`] wrote into `user_data`.
///
/// # Errors
///
/// `user_data` is no message that [`user_data`] wrote, or its start doesn't parse. The message
/// says where, never what the text holds.
pub fn start_in(user_data: &str) -> Result<Start, String> {
    let Some(parts) = user_data.strip_prefix(&header()) else {
        return Err(format!(
            "it doesn't begin with the header that names the boundary `{BOUNDARY}`: expected \
             the user data of a Pumpkin sandbox"
        ));
    };
    let Some((_, start)) = parts.split_once(&start_part()) else {
        return Err(format!(
            "it holds no part of type `{START_TYPE}` that begins `{START_LINE}`: expected the \
             user data of a Pumpkin sandbox"
        ));
    };
    let Some((start, _)) = start.split_once(&format!("\n--{BOUNDARY}")) else {
        return Err("its start's part has no end: expected the boundary after it".to_owned());
    };
    serde_json::from_str(start).map_err(|error| {
        format!(
            "its start can't be read, at line {} of the start, column {}: expected a start's \
             JSON, as the provider writes it",
            error.line(),
            error.column()
        )
    })
}

/// The lines of `console`, an instance's serial console, that its boot wrote, each from its
/// [`CONSOLE_PREFIX`] on, as another writer, such as a getty's prompt, may have begun the line.
pub fn boot_lines(console: &str) -> impl Iterator<Item = &str> {
    (console.lines()).filter_map(|line| line.find(CONSOLE_PREFIX).map(|at| &line[at..]))
}

fn header() -> String {
    format!(
        "Content-Type: multipart/mixed; boundary=\"{BOUNDARY}\"\n\
         MIME-Version: 1.0\n\
         \n\
         --{BOUNDARY}\n\
         Content-Type: text/cloud-config\n\
         \n"
    )
}

fn start_part() -> String {
    format!("--{BOUNDARY}\nContent-Type: {START_TYPE}\n\n{START_LINE}\n")
}
