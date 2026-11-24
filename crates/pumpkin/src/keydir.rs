//! A team's members' keys as a directory of files, as gitolite's admin repository keeps its users'
//! keys (`keydir/<user>.pub`): `<member>.pub` holds a line for each of the member's keys, and its
//! blank lines are skipped. The members' list that deploy/aws writes of such a directory is parsed
//! as [`Members`] parses it on the controller.
// https://github.com/sitaramc/gitolite/blob/f073598c5dca815a5553cebaee3d2a3f7abcbaec/src/triggers/post-compile/ssh-authkeys#L49

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use members::Members;
use stdx::Context as _;

use crate::cli::Exit;

/// Checks the keys of the members' files in `dir`, as the controller parses them. Returns 0.
pub(crate) fn check(dir: &Path) -> Result<Exit, String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).context(dir.display())? {
        let path = entry.context(dir.display())?.path();
        if path.extension() == Some(OsStr::new("pub")) && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    let mut keys = Vec::new();
    for path in &files {
        let text = fs::read_to_string(path).context(path.display())?;
        // A name that isn't UTF-8 is refused as any other name is.
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if !line.is_empty() {
                let at = format!("{}, line {}", path.display(), index + 1);
                keys.push((at, name.clone(), line.to_owned()));
            }
        }
    }
    let keys = keys
        .iter()
        .map(|(at, name, key)| (at.clone(), &**name, key.as_str()));
    Members::of_keys(keys)?;
    Ok(Exit::Success)
}
