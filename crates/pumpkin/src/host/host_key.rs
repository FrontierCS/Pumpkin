//! The host's ed25519 host key, which the install publishes in the SSM parameter that
//! `deploy/aws` makes for it ([`parameter`]).

use std::fs;

use aws::{Bound, Cli, Region};
use stdx::Context as _;

use crate::controller::Name;
use crate::controller::host_key::{HostKey, parameter};
use crate::iam::HOST;
use crate::say;

/// The public half of `sshd`'s ed25519 host key.
// cloud-init writes it at the first boot, before the user data's commands run:
// https://github.com/canonical/cloud-init/blob/905d10891051496bd196798b2a8e0d77f161db74/cloudinit/config/cc_ssh.py#L256-L261
const PUBLIC_KEY: &str = "/etc/ssh/ssh_host_ed25519_key.pub";

/// Publishes the host key of controller `name` in its parameter in `region`, unless the parameter
/// holds it already. Says so if it published.
pub(super) fn publish(region: &Region, name: &Name) -> Result<(), String> {
    let text = fs::read_to_string(PUBLIC_KEY).context(PUBLIC_KEY)?;
    let key = HostKey::parse(&text).ok_or_else(|| {
        format!(
            "{PUBLIC_KEY} holds no ed25519 public key: expected `ssh-ed25519 <blob>`, as \
             `ssh-keygen -t ed25519` writes it"
        )
    })?;
    let parameter = parameter(name);
    let cli = Cli::connect(region.clone())?;
    let held = crate::job::parameter_value(&cli, &HOST.published, &parameter)?;
    if held == key.as_str() {
        return Ok(());
    }
    let put = ["--name", &parameter, "--value", key.as_str(), "--overwrite"];
    cli.run(&HOST.publish, &put, Bound::Call)?;
    say(format_args!(
        "published {PUBLIC_KEY} in SSM parameter `{parameter}`"
    ));
    Ok(())
}
