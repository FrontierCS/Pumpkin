//! The contributor tools that xtask runs, each pinned to one version: those that cargo installs,
//! and those that their projects release as a file, which xtask fetches into the target
//! directory.

use std::fmt::Write as _;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::{env, process};

use sha2::{Digest as _, Sha256};
use xshell::{Cmd, Shell, cmd};

use crate::{Cargo, Result};

/// A tool that cargo installs and runs as a subcommand.
#[derive(Clone, Copy)]
pub(crate) struct Tool {
    /// The crate, `cargo-<subcommand>`, which also begins the line `--version` prints.
    binary: &'static str,
    version: &'static str,
}

pub(crate) const NEXTEST: Tool = Tool {
    binary: "cargo-nextest",
    version: "0.9.151",
};

pub(crate) const MUTANTS: Tool = Tool {
    binary: "cargo-mutants",
    version: "27.1.0",
};

const TOOLS: [Tool; 2] = [NEXTEST, MUTANTS];

/// A tool that its project releases as a file for Linux on x86-64, the one platform that
/// `pumpkin` is built on, pinned by the file's URL and SHA-256, and fetched by [`fetched`].
#[derive(Clone, Copy)]
pub(crate) struct Release {
    name: &'static str,
    version: &'static str,
    url: &'static str,
    sha256: &'static str,
    /// The tool's path in the file, if the file is a tar archive.
    member: Option<&'static str>,
}

/// Bazel's formatter of Starlark, which `pumpkin fmt` and `pumpkin check` run beside `pumpkin`.
pub(crate) const BUILDIFIER: Release = Release {
    name: "buildifier",
    version: "10.2.0",
    url: "https://github.com/bazel-contrib/buildtools/releases/download/v10.2.0/buildifier-linux-amd64",
    sha256: "470844069d710d0b7740e55007071c63e4d00be89a833dce5d1a91790e84dc28",
    member: None,
};

/// TypeScript's compiler, a native binary with its library's declarations beside it.
pub(crate) const TSC: Release = Release {
    name: "tsc",
    version: "7.0.2",
    url: "https://github.com/microsoft/TypeScript/releases/download/v7.0.2/typescript-linux-x64.tgz",
    sha256: "7ecad6f67377e831856367ab062ef394f21506a611405bf8ac0ff039348637d3",
    member: Some("package/lib/tsc"),
};

const ZIG: Release = Release {
    name: "zig",
    version: "0.16.0",
    url: "https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz",
    sha256: "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
    member: Some("zig-x86_64-linux-0.16.0/zig"),
};

const RELEASES: [Release; 3] = [BUILDIFIER, TSC, ZIG];

/// Sets the musl C compiler for a build of `pumpkin`. The host compiler still links the binary.
pub(crate) fn musl_cc<'a>(cargo: &Cargo, build: Cmd<'a>) -> Result<Cmd<'a>> {
    let zig = fetched(cargo, ZIG)?;
    Ok(build
        .env("ZIG_LIB_DIR", zig.with_file_name("lib"))
        .env("ZIG_GLOBAL_CACHE_DIR", zig.with_file_name("cache"))
        .env(
            "CC_x86_64_unknown_linux_musl",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("zig-cc"),
        ))
}

/// Installs each of [`TOOLS`] that cargo doesn't run at its version, into cargo's install root,
/// and fetches each of [`RELEASES`] that isn't fetched yet.
pub(crate) fn install(cargo: &Cargo) -> Result<()> {
    for release in RELEASES {
        let path = fetched(cargo, release)?;
        println!(
            "xtask: {} {} is at {}",
            release.name,
            release.version,
            path.display()
        );
    }
    for tool in TOOLS {
        let Tool { binary, version } = tool;
        if installed(cargo, tool).as_deref() == Some(version) {
            println!("xtask: {binary} {version} is installed");
            continue;
        }
        cargo
            .cmd(&["install", "--locked", &format!("{binary}@{version}")])
            .run()?;
    }
    Ok(())
}

/// Fails unless cargo runs `tool` at its version, saying how to install it.
pub(crate) fn require(cargo: &Cargo, tool: Tool) -> Result<()> {
    let Tool { binary, version } = tool;
    let wrong = match installed(cargo, tool) {
        Some(found) if found == version => return Ok(()),
        Some(found) => format!("{binary} is at {found}, not at {version}"),
        None => format!("{binary} {version} isn't installed"),
    };
    Err(format!("{wrong}: install it with `cargo xtask install-tools`").into())
}

/// The version of `tool` that cargo runs, as its `--version` prints it: `None` if cargo runs none,
/// or it prints no version.
fn installed(cargo: &Cargo, tool: Tool) -> Option<String> {
    let subcommand = tool.binary.strip_prefix("cargo-")?;
    let printed = cargo
        .cmd(&[subcommand, "--version"])
        .ignore_stderr()
        .read()
        .ok()?;
    match printed.split_whitespace().collect::<Vec<_>>()[..] {
        [binary, version, ..] if binary == tool.binary => Some(version.to_owned()),
        _ => None,
    }
}

/// The pinned `release` in the target directory, which cargo's configuration places, if it is
/// fetched there; else fails, saying how to fetch it.
pub(crate) fn found(cargo: &Cargo, release: Release) -> Result<PathBuf> {
    let path = path(cargo, release)?;
    if path.is_file() {
        return Ok(path);
    }
    let Release { name, version, .. } = release;
    Err(format!("{name} {version} isn't fetched: fetch it with `cargo xtask install-tools`").into())
}

/// Where the pinned `release` is, or is to be, in the target directory.
fn path(cargo: &Cargo, release: Release) -> Result<PathBuf> {
    let Release {
        name,
        version,
        url,
        member,
        ..
    } = release;
    let (os, arch) = (env::consts::OS, env::consts::ARCH);
    if (os, arch) != ("linux", "x86_64") {
        return Err(format!(
            "{name} is pinned for Linux on x86-64 alone, not for {os} on {arch}: build Pumpkin \
             there"
        )
        .into());
    }
    let file = url.rsplit('/').next().unwrap_or(name);
    let dir = target_dir(cargo)?.join(name).join(version);
    Ok(dir.join(member.unwrap_or(file)))
}

/// The pinned `release`, in the target directory, which cargo's configuration places. It is
/// fetched there with `curl` once, and kept, unpacked if it is a tar, only if its SHA-256 is the
/// pin's, so that what is there is always the pinned release.
pub(crate) fn fetched(cargo: &Cargo, release: Release) -> Result<PathBuf> {
    let Release {
        name,
        version,
        url,
        member,
        ..
    } = release;
    let path = path(cargo, release)?;
    if path.is_file() {
        return Ok(path);
    }
    let file = url.rsplit('/').next().unwrap_or(name);
    let dir = target_dir(cargo)?.join(name).join(version);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    eprintln!("xtask: fetching {name} {version} from {url}");
    // Names of this process's own, so that another fetch at once writes its own files.
    let part = dir.join(format!("{file}.{}.part", process::id()));
    let fetched = fetch(&cargo.sh, release, &part).and_then(|()| match member {
        None => fs::rename(&part, &path)
            .map_err(|error| format!("renaming {} failed: {error}", part.display()).into()),
        Some(member) => unpack(&cargo.sh, &part, &dir, member),
    });
    // Renamed, or unpacked and no longer needed, or failed.
    let _ = fs::remove_file(&part);
    fetched?;
    Ok(path)
}

/// Downloads `release` into `to`, executable, if its SHA-256 is the pin's.
fn fetch(sh: &Shell, release: Release, to: &Path) -> Result<()> {
    let Release {
        name,
        version,
        url,
        sha256,
        member: _,
    } = release;
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
             {name} {version}"
        )
        .into());
    }
    fs::set_permissions(to, Permissions::from_mode(0o755))
        .map_err(|error| format!("making {} executable failed: {error}", to.display()).into())
}

/// Unpacks the tar `archive` into `dir`, through a directory of this process's own, so
/// that `member`'s first directory appears whole or not at all.
fn unpack(sh: &Shell, archive: &Path, dir: &Path, member: &str) -> Result<()> {
    let Some(top) = Path::new(member).components().next() else {
        unreachable!("the member `{member}` has no component, though each pin names a path")
    };
    let unpacking = dir.join(format!("unpacking.{}", process::id()));
    fs::create_dir_all(&unpacking)
        .map_err(|error| format!("making {} failed: {error}", unpacking.display()))?;
    let tar = cmd!(sh, "tar --extract --file {archive} --directory {unpacking}");
    let unpacked = tar.run().map_err(Into::into).and_then(|()| {
        let to = dir.join(top);
        match fs::rename(unpacking.join(top), &to) {
            Ok(()) => Ok(()),
            // A concurrent fetch installed the archive first. Its compiler may already be in use.
            Err(_) if dir.join(member).is_file() => Ok(()),
            Err(error) => Err(format!("renaming into {} failed: {error}", to.display()).into()),
        }
    });
    let _ = fs::remove_dir_all(&unpacking);
    unpacked
}

/// The target directory, as `cargo metadata` reports it.
fn target_dir(cargo: &Cargo) -> Result<PathBuf> {
    let metadata = cargo.cmd(&["metadata", "--format-version=1", "--no-deps", "--locked"]);
    let metadata: serde_json::Value = serde_json::from_str(&metadata.read()?)?;
    match metadata["target_directory"].as_str() {
        Some(dir) => Ok(dir.into()),
        None => Err("cargo metadata reported no target_directory: expected its path".into()),
    }
}
