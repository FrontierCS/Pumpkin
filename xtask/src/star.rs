//! The pinned formatter of `.star` files, buildifier, which `pumpkin fmt` and `pumpkin check` run
//! beside `pumpkin`, and `ci`'s check that the repository's own `.star` files are formatted.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use stdx::process::{self, Environment};

use crate::repo::{Repo, STEP_TIMEOUT};
use crate::{Result, ran, step, tidy, tools};

/// Where the `.star` files are that [`check`] leaves as they are: the language's test fixtures,
/// whose expected outputs pin lines and columns, and the conformance files of other
/// implementations, which stay as their authors wrote them.
const UNCHECKED: &str = "crates/lang/tests/";

/// Where pumpkin's tests find the formatter, under any command that runs them: the link that
/// [`link`] makes, which `.cargo/config.toml` names.
const LINK: &str = env!("PUMPKIN_BUILDIFIER");

/// The pinned buildifier, fetched on first use ([`tools::fetched`]).
pub(crate) fn buildifier() -> Result<PathBuf> {
    tools::fetched(tools::BUILDIFIER)
}

/// Replaces what is at [`LINK`] with the pinned buildifier, and returns that path. It fetches
/// buildifier first if the cache lacks it.
pub(crate) fn link() -> Result<PathBuf> {
    let link = Path::new(LINK);
    let Some(dir) = link.parent() else {
        return Err(format!("PUMPKIN_BUILDIFIER names `{LINK}`: give the path of a file").into());
    };
    let buildifier = buildifier()?;
    fs::create_dir_all(dir).map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    // The tests hard-link it again, so it must lie on the checkout's filesystem.
    tools::replace(link, |made| linked(&buildifier, made))?;
    Ok(link.to_owned())
}

/// Makes `to` a hard link to `from`, or a copy where the two lie on different filesystems.
fn linked(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Err(error) if error.kind() == ErrorKind::CrossesDevices => fs::copy(from, to).map(drop),
        linked => linked,
    }
}

/// Puts `buildifier` beside `pumpkin`, where that `pumpkin` runs it, as `cargo xtask build` does
/// for an installed one. A link to it, or a copy, replaces what is there.
pub(crate) fn place(buildifier: &Path, pumpkin: &Path) -> Result<()> {
    let beside = pumpkin.with_file_name(install::File::Buildifier.name());
    match fs::remove_file(&beside) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            return Err(format!("removing {} failed: {error}", beside.display()).into());
        }
        Ok(()) | Err(_) => {}
    }
    linked(buildifier, &beside)
        .map_err(|error| format!("linking {} failed: {error}", beside.display()).into())
}

/// Fails unless each of the repository's `.star` files, tracked or not yet, but not ignored, and
/// but those below [`UNCHECKED`], is formatted, as `pumpkin fmt --check` of `pumpkin`, beside
/// which [`place`] has put the formatter, says.
pub(crate) fn check(repo: &Repo<'_>, pumpkin: &Path) -> Result<()> {
    let files = tidy::files(repo, "star")?.into_iter();
    let files: Vec<String> = files.filter(|path| !path.starts_with(UNCHECKED)).collect();
    // With no file named, `fmt` would format the working directory, the fixtures included.
    if files.is_empty() {
        return Ok(());
    }
    let mut fmt = process::command(pumpkin, Environment::Inherited);
    fmt.current_dir(repo.root)
        .args(["fmt", "--check", "--"])
        .args(files);
    ran(&mut fmt, step(STEP_TIMEOUT).named("pumpkin fmt")).map(drop)
}
