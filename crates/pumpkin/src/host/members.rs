//! The members' keys. The controller's admin keeps the list in the SSM parameter or the file that
//! the settings' `[deployment] members` names. This module copies the list into [`MEMBERS`]. `sshd`
//! admits only the keys in that file for the controller's user, each with a forced command that
//! names its member ([`Members::authorized_keys`]). A list that fails to parse changes nothing.

use std::path::Path;

use members::Members;

use crate::cli::Exit;
use crate::iam::HOST;
use crate::job::{Deployment, Source};

const DIR: &str = "/etc/ssh/pumpkin";

/// The file of the members' keys, which `sshd` reads.
pub(super) const MEMBERS: &str = "/etc/ssh/pumpkin/members";

/// Copies the members' keys, as the module says, for the controller whose settings the host file
/// names. Returns 0.
pub(crate) fn members() -> Result<Exit, String> {
    copy(&super::target()?.settings())?;
    Ok(Exit::Success)
}

/// Copies the members' keys, as the module says, for the controller whose settings are in
/// `settings`. Says so if the keys changed.
pub(super) fn copy(settings: &Source) -> Result<(), String> {
    let Deployment { name, members } = deployment(settings)?;
    let list = members.read(&HOST.members)?;
    let keys = Members::parse(&list).map_err(|why| {
        format!(
            "the list of controller `{name}`'s members, {members}, is refused: {why}; none of \
             its keys is copied"
        )
    })?;
    // Searchable by the controller's user, as `sshd` reads the file as that user.
    super::searchable(Path::new(DIR))?;
    super::wrote(Path::new(MEMBERS), keys.authorized_keys().as_bytes(), 0o644)?;
    Ok(())
}

/// The `[deployment]` of the controller's settings in `settings`.
fn deployment(settings: &Source) -> Result<Deployment, String> {
    let value = settings.read(&HOST.settings)?;
    let named = format!("the controller's settings, {settings},");
    let tables = crate::job::settings_tables(&value, &named);
    let tables = tables.map_err(|error| format!("{error}: correct them"));
    let deployment = tables.and_then(|mut tables| crate::job::deployment(&mut tables, &named));
    let (_, deployment) = deployment
        .map_err(|error| format!("{error}; until then, the members' keys stay as last copied"))?;
    Ok(deployment)
}
