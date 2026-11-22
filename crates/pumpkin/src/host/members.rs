//! The members' keys. The controller's admin keeps the list in the SSM parameter that the
//! settings' `[deployment] members` names. This module copies the list into [`MEMBERS`]. `sshd`
//! admits only the keys in that file for the controller's user, each with a forced command that
//! names its member ([`Members::authorized_keys`]). A list that fails to parse changes nothing.

use std::path::Path;

use aws::{Cli, ParameterArn};
use members::Members;

use crate::cli::Exit;
use crate::iam::HOST;
use crate::job::Deployment;

/// The directory of [`MEMBERS`].
const DIR: &str = "/etc/ssh/pumpkin";

/// The file of the members' keys, which `sshd` reads.
pub(super) const MEMBERS: &str = "/etc/ssh/pumpkin/members";

/// Copies the members' keys, as the module says, for the controller whose settings the host file
/// names. Returns 0.
pub(crate) fn members() -> Result<Exit, String> {
    copy(&super::settings()?)?;
    Ok(Exit::Success)
}

/// Copies the members' keys, as the module says, for the controller whose settings are SSM
/// parameter `settings`. Says so if the keys changed.
pub(super) fn copy(settings: &ParameterArn) -> Result<(), String> {
    let Deployment { name, members } = deployment(settings)?;
    let cli = Cli::connect(members.region().clone())?;
    let list = crate::job::parameter_value(&cli, &HOST.members, members.as_str())?;
    let keys = Members::parse(&list).map_err(|why| {
        format!(
            "the list of controller `{name}`'s members, SSM parameter `{members}`, is refused: \
             {why}; none of its keys is copied"
        )
    })?;
    // Searchable by the controller's user, as `sshd` reads the file as that user.
    super::searchable(Path::new(DIR))?;
    super::wrote(Path::new(MEMBERS), keys.authorized_keys().as_bytes(), 0o644)?;
    Ok(())
}

/// The `[deployment]` of the controller's settings in SSM parameter `settings`.
fn deployment(settings: &ParameterArn) -> Result<Deployment, String> {
    let cli = Cli::connect(settings.region().clone())?;
    let value = crate::job::parameter_value(&cli, &HOST.settings, settings.as_str())?;
    let named = format!("the controller's settings, SSM parameter `{settings}`,");
    let tables = crate::job::settings_tables(&value, &named);
    let tables = tables.map_err(|error| format!("{error}: correct them"));
    let deployment = tables.and_then(|mut tables| crate::job::deployment(&mut tables, &named));
    let (_, deployment) = deployment
        .map_err(|error| format!("{error}; until then, the members' keys stay as last copied"))?;
    Ok(deployment)
}
