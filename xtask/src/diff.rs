//! The working tree's changes since a revision, as one diff.

use std::path::Path;

use xshell::{Shell, cmd};

use crate::Result;

/// The diff of the working tree against the merge base of `rev` and `HEAD`. Fails if the working
/// tree holds an untracked Rust file that isn't ignored, which the diff would leave out, saying
/// to add it and run `again`.
pub(crate) fn since(sh: &Shell, rev: &str, again: &str) -> Result<String> {
    let others = cmd!(sh, "git ls-files -z --others --exclude-standard").read()?;
    let untracked: Vec<String> = (others.split('\0'))
        .filter(|path| Path::new(path).extension() == Some("rs".as_ref()))
        .map(|path| sh.current_dir().join(path).display().to_string())
        .collect();
    if !untracked.is_empty() {
        let untracked = untracked.join(" ");
        return Err(format!(
            "the diff would leave out untracked Rust files: add them to it with \
             `git add --intent-to-add -- {untracked}`, then run `{again}` again"
        )
        .into());
    }
    let base = cmd!(sh, "git merge-base {rev} HEAD").read()?;
    let mut diff = cmd!(sh, "git diff {base}").read()?;
    // `read` drops the newline that ends the diff's last line.
    diff.push('\n');
    Ok(diff)
}
