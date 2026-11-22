//! The controller's host key, which the controller publishes in an SSM parameter of its deployment
//! ([`parameter`]). Only the controller's role and the deployment's admin may write it there, so a
//! client pins the key that the parameter holds, in a file of the controller's own. Where the
//! parameter can't be read or holds no key, as for a deployment of an older release, the client
//! pins the key that the controller shows first.
// EC2 Instance Connect's package publishes an instance's host keys to AWS as it boots, with the
// instance's own credentials, and `tailscale ssh` pins the host keys that its control plane gives,
// in a known_hosts file of its own that it writes whole, with strict checking:
// https://github.com/aws/aws-ec2-instance-connect-config/blob/47de50509ed43f0c294513841739afb059d5900e/src/bin/eic_harvest_hostkeys#L81-L86
// https://github.com/aws/aws-ec2-instance-connect-config/blob/47de50509ed43f0c294513841739afb059d5900e/src/bin/eic_harvest_hostkeys#L164-L166
// https://github.com/tailscale/tailscale/blob/7708ab68c0f2a5f76e01646177ed6e84041c55f8/cmd/tailscale/cli/ssh.go#L101-L105
// https://github.com/tailscale/tailscale/blob/7708ab68c0f2a5f76e01646177ed6e84041c55f8/cmd/tailscale/cli/ssh.go#L147-L164

use std::path::{Path, PathBuf};

use aws::{Cli, Region};
use stdx::Context as _;

use super::Name;
use crate::iam::CLIENT;
use crate::{say, whole};

// The length of an ed25519 public key's blob in base64: 51 bytes, unpadded.
const BLOB_CHARS: usize = 68;

/// The name of the SSM parameter of controller `name`'s host key, which `deploy/aws` makes.
pub(crate) fn parameter(name: &Name) -> String {
    format!("/pumpkin-{name}/host-key")
}

/// An ed25519 public key, `ssh-ed25519 <blob>`, with no comment.
pub(crate) struct HostKey(String);

impl HostKey {
    /// Parses an OpenSSH public key, `ssh-ed25519 <blob> [comment]`, and drops its comment.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut words = text.trim_end().splitn(3, ' ');
        let (kind, blob) = (words.next(), words.next());
        let base64 = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/';
        let blob = blob.filter(|blob| blob.len() == BLOB_CHARS && blob.bytes().all(base64))?;
        (kind == Some("ssh-ed25519")).then(|| Self(format!("ssh-ed25519 {blob}")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `ssh` is to check the key that controller `name` in `region` published, which is pinned
/// below `kept` for `alias`. If none is pinned, pins the one that the controller publishes. If the
/// parameter can't be read or holds no key, says why, and returns false.
pub(super) fn pinned(
    kept: &Path,
    alias: &str,
    region: &Region,
    name: &Name,
) -> Result<bool, String> {
    let path = file(kept, alias);
    if path.try_exists().context(path.display())? {
        return Ok(true);
    }
    match published(region, name) {
        Ok(key) => pin(kept, alias, &key).map(|_| true),
        Err(error) => {
            say(format_args!(
                "controller `{name}`'s published host key can't be read: {error}: pinning the \
                 key it shows first"
            ));
            Ok(false)
        }
    }
}

/// The host key that controller `name` in `region` published, or why none can be read.
pub(super) fn published(region: &Region, name: &Name) -> Result<HostKey, String> {
    let parameter = parameter(name);
    let cli = Cli::connect(region.clone())?;
    let value = crate::job::parameter_value(&cli, &CLIENT.host_key, &parameter)
        .map_err(|error| format!("reading SSM parameter `{parameter}` failed: {error}"))?;
    HostKey::parse(&value).ok_or_else(|| {
        format!(
            "SSM parameter `{parameter}` holds no ed25519 public key: expected \
             `ssh-ed25519 <blob>`, which the controller's install writes"
        )
    })
}

/// The `known_hosts` file below `kept` that pins the key that the controller of `alias` published.
pub(super) fn file(kept: &Path, alias: &str) -> PathBuf {
    kept.join(format!("{alias}.known_hosts"))
}

/// Pins `key` for `alias` in [`file()`] below `kept`. Returns true unless it pinned `key` already.
pub(super) fn pin(kept: &Path, alias: &str, key: &HostKey) -> Result<bool, String> {
    let path = file(kept, alias);
    let line = format!("{alias} {}\n", key.as_str());
    // Public, as `key.pub` is.
    whole::converge(&path, line.as_bytes(), 0o644).context("pinning the controller's host key")
}
