//! The record's `sources/`: the text of each file that the workflow's evaluation read, from which
//! the run's bodies are evaluated, below `task/` or `library/` by its label's root, at its path.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use schema::ir::{Label, Root};
use stdx::Context;

use crate::{Durable, Host};

/// Writes `sources` into `dir`'s `sources/`, each file synced.
pub(super) fn write(
    host: &impl Host,
    dir: &Path,
    sources: &BTreeMap<Label, String>,
) -> Result<(), String> {
    for (label, text) in sources {
        let root = match label.root() {
            Root::Task => "task",
            Root::Library => "library",
        };
        let path = dir.join("sources").join(root).join(label.path());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context(parent.display())?;
        }
        let written = File::create(&path).and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            host.sync(&file, Durable::All)
        });
        written.context(path.display())?;
    }
    Ok(())
}
