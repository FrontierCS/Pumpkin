//! The contributor tools that xtask runs, each pinned to one version: the nightly rustfmt, which
//! rustup installs, and the tools that their projects release as a file, pinned by its URL and
//! SHA-256. A command fetches each tool it runs on first use, as rustup installs the toolchain that
//! `rust-toolchain.toml` pins when a command first needs it:
//! <https://github.com/rust-lang/rustup/blob/bb60b1e89cc01a682dd16f2854329030938d3fe3/src/config.rs#L810-L812>.

use std::fmt::Write as _;
use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, io, process};

use sha2::{Digest as _, Sha256};
use stdx::process::Environment;
use xshell::{Cmd, Shell, cmd};

use crate::{Cargo, Result};

/// The toolchain whose `rustfmt` formats the workspace: `rustfmt.toml` sets nightly-only options.
pub(crate) const RUSTFMT_TOOLCHAIN: &str = "nightly-2026-11-07";

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

/// The static build, whose SHA-256 is the one in the `.sha256` file that nextest publishes beside
/// it.
pub(crate) const NEXTEST: Release = Release {
    name: "cargo-nextest",
    version: "0.9.151",
    url: "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-0.9.151/cargo-nextest-0.9.151-x86_64-unknown-linux-musl.tar.gz",
    sha256: "6b284d6fde9670dfb6ee10ea9d9abbc85f95643d3f592472d420d82e257b3a6c",
    member: Some("cargo-nextest"),
};

pub(crate) const MUTANTS: Release = Release {
    name: "cargo-mutants",
    version: "27.1.0",
    url: "https://github.com/sourcefrog/cargo-mutants/releases/download/v27.1.0/cargo-mutants-x86_64-unknown-linux-gnu.tar.gz",
    sha256: "dfe6dc37d0342c891d2829b5a695aa57c2d0edecef7e7d0399a30cc6e206411e",
    member: Some("cargo-mutants"),
};

const RELEASES: [Release; 5] = [BUILDIFIER, TSC, ZIG, NEXTEST, MUTANTS];

/// The name of the link to xtask that cc-rs runs as the musl C compiler ([`musl_cc`]).
const MUSL_CC: &str = "x86_64-linux-musl-cc";

/// Sets the musl C compiler for a build of `pumpkin`, the pinned Zig's. The host compiler
/// still links the binary.
pub(crate) fn musl_cc<'a>(cargo: &Cargo, build: Cmd<'a>) -> Result<Cmd<'a>> {
    let zig = fetched(cargo, ZIG)?;
    // The checkout's own, as the link to xtask is: the cache holds each release as it was fetched.
    let dir = target_dir(cargo)?.join("musl-cc");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    // One executable, xtask as `MUSL_CC`: cc-rs drops the arguments of a compiler named as
    // `zig cc` from its flag and family checks, which then all fail.
    let cc = dir.join(MUSL_CC);
    let xtask = env::current_exe()
        .map_err(|error| format!("finding xtask's own executable failed: {error}"))?;
    replace(&cc, |made| symlink(&xtask, made))?;
    replace(&dir.join("zig"), |made| symlink(&zig, made))?;
    Ok(build
        .env("ZIG_LIB_DIR", zig.with_file_name("lib"))
        .env("ZIG_GLOBAL_CACHE_DIR", dir.join("cache"))
        .env("CC_x86_64_unknown_linux_musl", cc))
}

/// Runs the pinned Zig's C compiler with this process's arguments, if xtask runs as
/// [`MUSL_CC`], and returns how that failed; otherwise returns `None`.
pub(crate) fn run_as_musl_cc() -> Option<ExitCode> {
    let mut args = env::args_os();
    let cc = PathBuf::from(args.next()?);
    if cc.file_name()? != MUSL_CC {
        return None;
    }
    let zig = cc.with_file_name("zig");
    // cc-rs passes `--target` in Rust's spelling: Zig takes the last, its own.
    // Zig finds its library and cache directories in the environment that cc-rs gives it.
    let error = stdx::process::command(&zig, Environment::Inherited)
        .arg("cc")
        .args(args)
        .args(["--target=x86_64-linux-musl", "-fno-sanitize=all"])
        .exec();
    eprintln!(
        "xtask: running {} failed: {error}: run `cargo xtask install-tools` to fetch it again",
        zig.display()
    );
    Some(ExitCode::FAILURE)
}

/// Replaces what is at `at` with the file that `write` makes at the path it is given.
pub(crate) fn replace(at: &Path, write: impl FnOnce(&Path) -> io::Result<()>) -> Result<()> {
    // Made beside it, then renamed over it, so that a process that runs it at once never finds
    // it missing.
    let mut made = at.as_os_str().to_owned();
    made.push(format!(".{}", process::id()));
    let made = PathBuf::from(made);
    let replaced = write(&made).and_then(|()| fs::rename(&made, at));
    if let Err(error) = replaced {
        // Made and not renamed, or not made.
        let _ = fs::remove_file(&made);
        return Err(format!("making {} failed: {error}", at.display()).into());
    }
    Ok(())
}

/// Installs the nightly rustfmt, and fetches each of [`RELEASES`], each unless it is already
/// there.
pub(crate) fn install(cargo: &Cargo) -> Result<()> {
    rustfmt(&cargo.sh)?;
    for release in RELEASES {
        let path = fetched(cargo, release)?;
        println!(
            "xtask: {} {} is at {}",
            release.name,
            release.version,
            path.display()
        );
    }
    Ok(())
}

/// Installs [`RUSTFMT_TOOLCHAIN`]'s rustfmt through rustup, in its minimal profile, unless rustup
/// already runs it.
pub(crate) fn rustfmt(sh: &Shell) -> Result<()> {
    let which = cmd!(sh, "rustup which --toolchain {RUSTFMT_TOOLCHAIN} rustfmt")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .quiet()
        .ignore_stdout()
        .ignore_stderr()
        .ignore_status();
    if which.output()?.status.success() {
        return Ok(());
    }
    let install = cmd!(
        sh,
        "rustup toolchain install {RUSTFMT_TOOLCHAIN} --profile minimal --component rustfmt"
    );
    Ok(install.run()?)
}

/// The pinned `release`, in the cache of one user, which every checkout shares. It is fetched
/// there with `curl` once, and kept, unpacked if it is a tar, only if its SHA-256 is the pin's,
/// so that what is there is always the pinned release.
pub(crate) fn fetched(cargo: &Cargo, release: Release) -> Result<PathBuf> {
    let Release {
        name,
        version,
        url,
        sha256,
        member,
    } = release;
    let cache = cache()?;
    // Keyed by the SHA-256, as Bazel's repository cache keys a download:
    // https://github.com/bazelbuild/bazel/blob/7108c77007efc6f3162e127f252707e9ab743d36/src/main/java/com/google/devtools/build/lib/bazel/repository/cache/RepositoryCache.java#L231-L236
    let entry = cache.join(sha256);
    let file = url.rsplit('/').next().unwrap_or(name);
    let path = entry.join(member.unwrap_or(file));
    if path.is_file() {
        return Ok(path);
    }
    let (os, arch) = (env::consts::OS, env::consts::ARCH);
    if (os, arch) != ("linux", "x86_64") {
        return Err(format!(
            "{name} is pinned for Linux on x86-64 alone, not for {os} on {arch}: build Pumpkin \
             there"
        )
        .into());
    }
    eprintln!("xtask: fetching {name} {version} from {url}");
    // A directory of this process's own, renamed whole into place once it holds the release, so
    // that a fetch at once, from another checkout too, never finds an entry half made.
    let staging = cache.join(format!(".{sha256}.{}", process::id()));
    // Left by a process that had this one's id and was killed.
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|error| format!("making {} failed: {error}", staging.display()))?;
    let download = staging.join(file);
    let fetched = fetch(&cargo.sh, release, &download)
        .and_then(|()| match member {
            None => Ok(()),
            Some(_) => unpack(&cargo.sh, &download, &staging),
        })
        .and_then(|()| match fs::rename(&staging, &entry) {
            Ok(()) => Ok(()),
            // A fetch at once put the release there first.
            Err(_) if path.is_file() => Ok(()),
            Err(error) => Err(format!(
                "renaming {} to {} failed: {error}: remove {} to fetch it again",
                staging.display(),
                entry.display(),
                entry.display()
            )
            .into()),
        });
    // Renamed, or not needed, or failed.
    let _ = fs::remove_dir_all(&staging);
    fetched?;
    Ok(path)
}

/// Where the fetched releases are: `pumpkin/tools` in the user's cache directory, as the XDG Base
/// Directory Specification places it, the directory where the launcher `deploy/pumpkin` caches
/// the releases of Pumpkin.
fn cache() -> Result<PathBuf> {
    // The specification has a relative path ignored.
    let dir = match env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => match env::var_os("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home).join(".cache"),
            _ => {
                return Err(
                    "no cache directory is set: set XDG_CACHE_HOME to an absolute path or set HOME"
                        .into(),
                );
            }
        },
    };
    Ok(dir.join("pumpkin/tools"))
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

/// Unpacks the tar `archive` into `dir`, then removes it.
fn unpack(sh: &Shell, archive: &Path, dir: &Path) -> Result<()> {
    cmd!(sh, "tar --extract --file {archive} --directory {dir}").run()?;
    fs::remove_file(archive)
        .map_err(|error| format!("removing {} failed: {error}", archive.display()).into())
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
