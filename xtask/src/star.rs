//! The pinned formatter of `.star` files, buildifier, which `pumpkin fmt` and `pumpkin check` run
//! beside `pumpkin`, and `ci`'s check that the repository's own `.star` files are formatted.

use std::env;
use std::fmt::Write as _;
use std::fs::{self, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use xshell::{Shell, cmd};

use crate::{Cargo, Result, tidy};

const VERSION: &str = "10.2.0";

/// The release's file for the platform that `pumpkin` runs on, by the names that Rust gives its
/// system and architecture, with the file's SHA-256: Linux on x86-64.
const PINS: [(&str, &str, &str, &str); 1] = [(
    "linux",
    "x86_64",
    "buildifier-linux-amd64",
    "470844069d710d0b7740e55007071c63e4d00be89a833dce5d1a91790e84dc28",
)];

/// Where the `.star` files are that [`check`] leaves as they are: the language's test fixtures,
/// whose expected outputs pin lines and columns, and the conformance files of other
/// implementations, which stay as their authors wrote them.
const UNCHECKED: &str = "crates/lang/tests/";

/// The pinned buildifier of this machine's platform, in the target directory, which cargo's
/// configuration places: fetched there once, and kept only if its SHA-256 is the pin's, so that
/// what is there is always the pinned release.
pub(crate) fn buildifier(cargo: &Cargo) -> Result<PathBuf> {
    let (os, arch) = (env::consts::OS, env::consts::ARCH);
    let pin = PINS
        .iter()
        .find(|(system, machine, ..)| (*system, *machine) == (os, arch));
    let Some((_, _, file, sha256)) = pin else {
        return Err(format!(
            "buildifier is pinned for Linux on x86-64 alone, not for {os} on {arch}: \
             build Pumpkin there"
        )
        .into());
    };
    let dir = target_dir(cargo)?.join("buildifier").join(VERSION);
    let path = dir.join(file);
    if path.is_file() {
        return Ok(path);
    }
    fs::create_dir_all(&dir)
        .map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    let url =
        format!("https://github.com/bazel-contrib/buildtools/releases/download/v{VERSION}/{file}");
    eprintln!("xtask: fetching buildifier {VERSION} from {url}");
    // A name of this process's own, so that another fetch at once writes its own file.
    let part = dir.join(format!("{file}.{}.part", std::process::id()));
    let fetched = fetch(&cargo.sh, &url, &part, sha256).and_then(|()| {
        fs::rename(&part, &path)
            .map_err(|error| format!("renaming {} failed: {error}", part.display()).into())
    });
    if fetched.is_err() {
        let _ = fs::remove_file(&part);
    }
    fetched?;
    Ok(path)
}

/// Downloads `url` into `to`, executable, if its SHA-256 is `sha256`.
fn fetch(sh: &Shell, url: &str, to: &Path, sha256: &str) -> Result<()> {
    let curl = cmd!(
        sh,
        "curl --location --silent --show-error --fail --proto =https --tlsv1.2 --output {to} {url}"
    );
    curl.run()?;
    let bytes =
        fs::read(to).map_err(|error| format!("reading {} failed: {error}", to.display()))?;
    let mut found = String::new();
    for byte in Sha256::digest(&bytes) {
        let _ = write!(found, "{byte:02x}");
    }
    if found != sha256 {
        return Err(format!(
            "{url} has the SHA-256 {found}, not the pinned {sha256}: the download is not \
             buildifier {VERSION}"
        )
        .into());
    }
    fs::set_permissions(to, Permissions::from_mode(0o755))
        .map_err(|error| format!("making {} executable failed: {error}", to.display()).into())
}

fn target_dir(cargo: &Cargo) -> Result<PathBuf> {
    let metadata = cargo.cmd(&["metadata", "--format-version=1", "--no-deps", "--locked"]);
    let metadata: serde_json::Value = serde_json::from_str(&metadata.read()?)?;
    match metadata["target_directory"].as_str() {
        Some(dir) => Ok(dir.into()),
        None => Err("cargo metadata reported no target_directory: expected its path".into()),
    }
}

/// Puts `buildifier` beside `pumpkin`, where that `pumpkin` runs it, as `cargo xtask install`
/// puts it beside an installed one: a link to it, which replaces what is there.
pub(crate) fn place(buildifier: &Path, pumpkin: &Path) -> Result<()> {
    let beside = pumpkin.with_file_name("buildifier");
    match fs::remove_file(&beside) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            return Err(format!("removing {} failed: {error}", beside.display()).into());
        }
        Ok(()) | Err(_) => {}
    }
    fs::hard_link(buildifier, &beside)
        .map_err(|error| format!("linking {} failed: {error}", beside.display()).into())
}

/// Fails unless each of the repository's `.star` files, tracked or not yet, but not ignored, and
/// but those below [`UNCHECKED`], is formatted, as `pumpkin fmt --check` of `pumpkin`, beside
/// which [`place`] has put the formatter, says.
pub(crate) fn check(sh: &Shell, pumpkin: &Path) -> Result<()> {
    let files = tidy::files(sh, "star")?.into_iter();
    let files: Vec<String> = files.filter(|path| !path.starts_with(UNCHECKED)).collect();
    // With no file named, `fmt` would format the working directory, the fixtures included.
    if files.is_empty() {
        return Ok(());
    }
    cmd!(sh, "{pumpkin} fmt --check -- {files...}").run()?;
    Ok(())
}
