//! What the EC2 provider and `ec2-boot`, the boot of each of its sandboxes' instances, share.
//!
//! The provider's user data writes a [`Start`] to [`START_FILE`], which `ec2-boot start` reads
//! and removes.

use std::net::SocketAddrV4;

use serde::{Deserialize, Serialize};

use crate::sandbox::RegistrationGrant;

/// Where `sandboxd` is in the image of it that a provider whose sandboxes pull from a registry
/// pushes.
pub const SANDBOXD_IN_IMAGE: &str = "/sandboxd";

/// Where `ec2-boot` is in that image.
pub const BOOT_IN_IMAGE: &str = "/ec2-boot";

/// The directory, in memory and root's alone once `ec2-boot` starts, that the user data makes on
/// an instance's first boot, and that holds [`START_FILE`] and the grant's file.
pub const GRANT_DIR: &str = "/run/pumpkin";

/// The file in [`GRANT_DIR`] that holds the [`Start`] as JSON until `ec2-boot` reads it.
pub const START_FILE: &str = "/run/pumpkin/start.json";

/// The start of each line that an instance's boot writes to its serial console.
pub const CONSOLE_PREFIX: &str = "pumpkin-ec2:";

/// The sandbox that `ec2-boot start` starts on its instance: the role's container, whose first
/// process is `sandboxd`, dialing [`gateway`](Self::gateway).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    /// The run's hard end, in whole seconds since the Unix epoch, rounded down. Once the
    /// container has started, the instance powers off then, and the boot's wait for the
    /// container ends by then.
    pub hard_end_unix_seconds: u64,
    /// The create's deadline, in whole seconds since the Unix epoch, rounded down, at most the
    /// hard end. Until the container has started, the instance powers off then, whatever hangs,
    /// and each command of the boot's is killed by then.
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
    /// The grant that `sandboxd` presents, in its environment alone.
    pub grant: RegistrationGrant,
    /// Whether the role makes ops (`provider::Create::ops`).
    pub ops: bool,
    /// Whether `sandboxd` serves the model endpoint
    /// (`provider::Create::model`).
    pub model: bool,
}
