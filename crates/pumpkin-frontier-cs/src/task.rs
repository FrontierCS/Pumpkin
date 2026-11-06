//! A ported task, as its directory gives it.
//!
//! A ported task's directory, named for its problem id, holds the files the port copies from its
//! source and the recipes of its images, written against Pumpkin's in-sandbox contract.

use std::path::{Path, PathBuf};

use pumpkin_schema::ir::Name;
use stdx::Context;

/// A task ported from FrontierCS, as its directory describes it.
#[derive(Debug)]
#[expect(
    dead_code,
    reason = "nothing in the crate reads a task's directory or problem id: `Task` is its \
              output, which `Debug` shows"
)]
pub struct Task {
    /// The directory, as [`Task::load`] was given it.
    dir: PathBuf,
    problem_id: Name,
}

impl Task {
    /// The task in `dir`, a ported task's directory: its problem id is the directory's name,
    /// which is all of the directory that `load` reads.
    ///
    /// # Errors
    ///
    /// Fails, naming the directory, if its name isn't a problem id.
    pub fn load(dir: &Path) -> Result<Self, String> {
        Ok(Self {
            dir: dir.to_owned(),
            problem_id: problem_id(dir)?,
        })
    }
}

/// The problem id of the task in `dir`: the directory's name.
fn problem_id(dir: &Path) -> Result<Name, String> {
    // The layout, `tasks/frontier-cs/<problem id>/`, already names each task by its directory,
    // so the id is that name rather than a value that a file could contradict.
    let Some(name) = dir.file_name() else {
        return Err(format!(
            "`{}` doesn't end in a directory's name: give the task's directory by a path that \
             ends in its name, the task's problem id",
            dir.display()
        ));
    };
    Name::try_from(name.to_string_lossy().into_owned()).context(format_args!(
        "{}: the directory's name, the task's problem id",
        dir.display()
    ))
}
