//! The file of a run's job directory, `jobs/<id>/member`, that names the member who started the
//! job, on one line. A job that a build from before members started has none.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use stdx::Context as _;

use crate::member::Member;

const FILE: &str = "member";

/// The member of the job whose directory is `job`, or `None` for one that names none.
pub(crate) fn member(job: &Path) -> Result<Option<Member>, String> {
    let path = job.join(FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(path.display()),
    };
    let name = text.strip_suffix('\n').unwrap_or(&text);
    let member = Member::try_from(name.to_owned()).context(path.display())?;
    Ok(Some(member))
}

/// Names `member` as the one of the job whose directory is `job`, whole or not at all, whenever
/// the machine fails: written and synced aside, renamed into place, then the directory synced,
/// as a run's record replaces a file.
pub(crate) fn write_member(job: &Path, member: &Member) -> Result<(), String> {
    let path = job.join(FILE);
    let mut aside = tempfile::Builder::new()
        .prefix(".member.")
        .permissions(fs::Permissions::from_mode(0o644))
        .tempfile_in(job)
        .context(job.display())?;
    let written = writeln!(aside, "{member}").and_then(|()| aside.as_file().sync_all());
    written.context(aside.path().display())?;
    aside.persist(&path).context(path.display())?;
    File::open(job)
        .and_then(|dir| dir.sync_all())
        .context(job.display())
}
