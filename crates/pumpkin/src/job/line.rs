//! A file of one line in a job's directory, which a reader finds whole or not at all.

use std::fs::{self, File};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use stdx::Context as _;

/// Writes `line` as the file `file` of the job's directory `job`. A reader finds the file whole or
/// not at all, whenever the machine fails.
// Written and synced aside, renamed into place, then the directory synced, as a run's record
// replaces a file.
pub(super) fn write(job: &Path, file: &str, line: &str) -> Result<(), String> {
    let path = job.join(file);
    let mut aside = tempfile::Builder::new()
        .prefix(&format!(".{file}."))
        .permissions(fs::Permissions::from_mode(0o644))
        .tempfile_in(job)
        .context(job.display())?;
    let written = writeln!(aside, "{line}").and_then(|()| aside.as_file().sync_all());
    written.context(aside.path().display())?;
    aside.persist(&path).context(path.display())?;
    File::open(job)
        .and_then(|dir| dir.sync_all())
        .context(job.display())
}
