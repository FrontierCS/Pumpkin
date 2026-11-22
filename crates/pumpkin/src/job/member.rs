//! The file of a run's job directory, `jobs/<id>/member`, that names the member who started the
//! job, on one line. Each job's start writes it before the job runs.

use std::path::Path;
use std::{fs, io};

use members::Member;
use stdx::Context as _;

const FILE: &str = "member";

/// The member of the job whose directory is `job`.
pub(crate) fn member(job: &Path) -> Result<Member, String> {
    let path = job.join(FILE);
    let text = fs::read_to_string(&path).context(path.display())?;
    let name = text.strip_suffix('\n').unwrap_or(&text);
    Member::try_from(name.to_owned()).context(path.display())
}

/// The member of the run whose job's directory is `job`, or `None` if no job started the run, as
/// no such directory shows.
pub(crate) fn run_member(job: &Path) -> Result<Option<Member>, String> {
    match fs::symlink_metadata(job) {
        Ok(_) => member(job).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(job.display()),
    }
}

/// Names `member` as the member of the job whose directory is `job`. A reader finds the file whole
/// or not at all, whenever the machine fails.
pub(crate) fn write_member(job: &Path, member: &Member) -> Result<(), String> {
    crate::whole::replace(&job.join(FILE), format!("{member}\n").as_bytes(), 0o644)
}
