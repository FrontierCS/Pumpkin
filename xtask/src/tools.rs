//! The contributor tools that xtask runs, each pinned to one version. rustup installs the nightly
//! rustfmt. Each other tool is a file that its project releases, pinned by its URL and SHA-256.
//!
//! A command fetches each tool it runs on first use. rustup does the same with the toolchain that
//! `rust-toolchain.toml` pins:
//! <https://github.com/rust-lang/rustup/blob/bb60b1e89cc01a682dd16f2854329030938d3fe3/src/config.rs#L810-L812>.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, io, process};

use sha2::{Digest as _, Sha256};
use stdx::process::{Bound, Echo, Environment, Failure, Run, Stream};

use crate::repo::STEP_TIMEOUT;
use crate::{Result, ran, root, step, target_dir};

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
    /// The tool's path in the file, if the file is an archive, a zip or a tar.
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

/// The static build. Its SHA-256 is the one in the `.sha256` file that nextest publishes beside
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

/// Terraform, which `ci` checks `deploy/aws` with, at the oldest version that the module's
/// `versions.tf` allows. Its SHA-256 is the one in the signed `SHA256SUMS` file that its release
/// publishes beside it.
pub(crate) const TERRAFORM: Release = Release {
    name: "terraform",
    version: "1.15.9",
    url: "https://releases.hashicorp.com/terraform/1.15.9/terraform_1.15.9_linux_amd64.zip",
    sha256: "76edd0b22d2f27d3d2e097cd793209646f719cf60f02ff3af626b07361137da1",
    member: Some("terraform"),
};

/// Terraform's AWS provider, at the oldest version that `deploy/aws/versions.tf` allows and the
/// root in `deploy/aws/examples/team` pins. Its SHA-256 is from its release's signed `SHA256SUMS`
/// too.
const AWS_PROVIDER: Release = Release {
    name: "terraform-provider-aws",
    version: "6.72.0",
    url: "https://releases.hashicorp.com/terraform-provider-aws/6.72.0/terraform-provider-aws_6.72.0_linux_amd64.zip",
    sha256: "2ffb6b2caaf320b61b406cff192d6e4303b64e32990b69384a056fe5af3d015b",
    member: Some("terraform-provider-aws_v6.72.0_x5"),
};

const RELEASES: [Release; 7] = [
    BUILDIFIER,
    TSC,
    ZIG,
    NEXTEST,
    MUTANTS,
    TERRAFORM,
    AWS_PROVIDER,
];

/// The name of the link to xtask that cc-rs runs as the musl C compiler ([`musl_cc`]).
const MUSL_CC: &str = "x86_64-linux-musl-cc";

/// Sets the musl C compiler for a static build, the pinned Zig's. The host compiler still links
/// the binary.
pub(crate) fn musl_cc(build: &mut Command) -> Result<()> {
    let zig = fetched(ZIG)?;
    // The directory belongs to the checkout, as the link to xtask does. The cache holds each
    // release only as it was fetched.
    let dir = target_dir()?.join("musl-cc");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    // One executable, xtask as `MUSL_CC`: cc-rs drops the arguments of a compiler named as
    // `zig cc` from its flag and family checks, which then all fail.
    let cc = dir.join(MUSL_CC);
    let xtask = env::current_exe()
        .map_err(|error| format!("finding xtask's own executable failed: {error}"))?;
    replace(&cc, |made| symlink(&xtask, made))?;
    replace(&dir.join("zig"), |made| symlink(&zig, made))?;
    build
        .env("ZIG_LIB_DIR", zig.with_file_name("lib"))
        .env("ZIG_GLOBAL_CACHE_DIR", dir.join("cache"))
        .env("CC_x86_64_unknown_linux_musl", cc);
    Ok(())
}

/// A directory that holds the pinned AWS provider in the layout of Terraform's `-plugin-dir`, from
/// which `terraform init` installs it without a registry.
pub(crate) fn terraform_plugins() -> Result<PathBuf> {
    let provider = fetched(AWS_PROVIDER)?;
    // The directory belongs to the checkout, as the musl C compiler's does.
    let plugins = target_dir()?.join("terraform-plugins");
    let dir = plugins
        .join("registry.terraform.io/hashicorp/aws")
        .join(AWS_PROVIDER.version);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("making {} failed: {error}", dir.display()))?;
    // In the unpacked layout, the platform's directory holds what the provider's zip holds, which
    // `init` hashes as the registry does, so that it matches the hash in a lock file that a
    // registry's `init` wrote. `init` links the directory into the module's `.terraform`:
    // https://github.com/hashicorp/web-unified-docs/blob/45e8f63984f52111e8efbb7610095c195d31427a/content/terraform/v1.15.x/docs/cli/config/config-file.mdx#L259-L270
    let Some(entry) = provider.parent() else {
        unreachable!(
            "{} has no parent, though `fetched` returns the release's cache entry joined with \
             its member, which names a file at the top of the zip",
            provider.display()
        )
    };
    replace(&dir.join("linux_amd64"), |made| symlink(entry, made))?;
    Ok(plugins)
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
        "xtask: running {} failed: {error}: remove the cache entry that it leads to, then run \
         `cargo xtask install-tools` to fetch it again",
        zig.display()
    );
    Some(ExitCode::FAILURE)
}

/// Replaces what is at `at` with the file that `write` makes at the path it is given.
pub(crate) fn replace(at: &Path, write: impl FnOnce(&Path) -> io::Result<()>) -> Result<()> {
    // The file is made beside `at` and then renamed over it, so that a process that runs it at
    // the same time never finds it missing.
    let mut made = at.as_os_str().to_owned();
    made.push(format!(".{}", process::id()));
    let made = PathBuf::from(made);
    let replaced = write(&made).and_then(|()| fs::rename(&made, at));
    if let Err(error) = replaced {
        // `write` may have made the file before the rename failed.
        let _ = fs::remove_file(&made);
        return Err(format!("making {} failed: {error}", at.display()).into());
    }
    Ok(())
}

/// Installs the nightly rustfmt and fetches each of [`RELEASES`]. It skips each one that is
/// already there.
pub(crate) fn install() -> Result<()> {
    rustfmt()?;
    for release in RELEASES {
        let path = fetched(release)?;
        println!(
            "xtask: {} {} is at {}",
            release.name,
            release.version,
            path.display()
        );
    }
    Ok(())
}

/// Installs [`RUSTFMT_TOOLCHAIN`]'s rustfmt through rustup in the minimal profile, unless rustup
/// already runs it.
pub(crate) fn rustfmt() -> Result<()> {
    let mut which = rustup(&["which", "--toolchain", RUSTFMT_TOOLCHAIN, "rustfmt"]);
    which.env("RUSTUP_AUTO_INSTALL", "0");
    let quiet = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent)
        .stdout(Stream::Discarded)
        .stderr(Stream::Discarded);
    match stdx::process::run(&mut which, quiet) {
        Ok(_) => return Ok(()),
        // rustup doesn't run that toolchain's rustfmt.
        Err(Failure::Exited { .. }) => {}
        Err(failure) => return Err(failure.to_string().into()),
    }
    let install = ["toolchain", "install", RUSTFMT_TOOLCHAIN];
    let mut install = rustup(&install);
    install.args(["--profile", "minimal", "--component", "rustfmt"]);
    ran(&mut install, step(STEP_TIMEOUT)).map(drop)
}

fn rustup(args: &[&str]) -> Command {
    let mut rustup = stdx::process::command("rustup", Environment::Inherited);
    rustup.current_dir(root()).args(args);
    rustup
}

/// The pinned `release` in the cache that all of one user's checkouts share. The first call
/// fetches it with `curl`. It keeps the file only if its SHA-256 is the pin's, unpacks it if it is
/// an archive, and makes it read-only.
pub(crate) fn fetched(release: Release) -> Result<PathBuf> {
    let Release {
        name,
        version,
        url,
        sha256,
        member,
    } = release;
    let cache = cache()?;
    // Each entry is keyed by its SHA-256, as Bazel's repository cache keys a download:
    // https://github.com/bazelbuild/bazel/blob/7108c77007efc6f3162e127f252707e9ab743d36/src/main/java/com/google/devtools/build/lib/bazel/repository/cache/RepositoryCache.java#L231-L236
    let entry = cache.join(sha256);
    let file = url.rsplit('/').next().unwrap_or(name);
    let path = entry.join(member.unwrap_or(file));
    if path.is_file() {
        return Ok(path);
    }
    let again = || format!("remove {} to fetch it again", entry.display());
    if entry.exists() {
        return Err(format!("{} lacks {name} {version}: {}", entry.display(), again()).into());
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
    fs::create_dir_all(&cache)
        .map_err(|error| format!("making {} failed: {error}", cache.display()))?;
    // The fetch fills a directory of its own and renames it into place once it holds the
    // release. Another fetch at the same time, even from another checkout, container or host,
    // never finds an entry half made. The name holds the time as well as the process ID, since
    // process IDs repeat across PID namespaces and hosts. Bazel names its temporary file by a
    // random UUID for the same reason.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let staging = format!(".{sha256}.{}.{}", process::id(), nanos.as_nanos());
    let staging = cache.join(staging);
    fs::create_dir(&staging)
        .map_err(|error| format!("making {} failed: {error}", staging.display()))?;
    let download = staging.join(file);
    let staged = staging.join(member.unwrap_or(file));
    let fetched = fetch(release, &download)
        .and_then(|()| match member {
            None => Ok(()),
            Some(_) => unpack(&download, &staging),
        })
        // Every checkout's hard link to the file shares its inode, so the file is read-only, as
        // Go's module cache makes what it unpacks:
        // https://github.com/golang/go/blob/46b62495e1bf4d56e0313320772ae588d9119aed/src/cmd/go/internal/modfetch/fetch.go#L501-L522
        .and_then(|()| {
            fs::set_permissions(&staged, Permissions::from_mode(0o555)).map_err(|error| {
                format!("{url} holds no {name} at {}: {error}", staged.display()).into()
            })
        })
        .and_then(|()| match fs::rename(&staging, &entry) {
            Ok(()) => Ok(()),
            // Another fetch at the same time put the release there first.
            Err(_) if path.is_file() => Ok(()),
            Err(error) => Err(format!(
                "renaming {} to {} failed: {error}: {}",
                staging.display(),
                entry.display(),
                again()
            )
            .into()),
        });
    // After the rename nothing is left here. Otherwise the directory is no longer needed.
    let _ = fs::remove_dir_all(&staging);
    fetched?;
    Ok(path)
}

/// Where the fetched releases are: `pumpkin/tools` in the user's cache directory, as the XDG Base
/// Directory Specification places it. The launcher `dist/pumpkin` caches Pumpkin's releases in
/// that cache directory too.
fn cache() -> Result<PathBuf> {
    // The specification says to ignore a relative path.
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

/// Downloads `release` into `to` and fails unless its SHA-256 is the pin's.
fn fetch(release: Release, to: &Path) -> Result<()> {
    let Release {
        name,
        version,
        url,
        sha256,
        member: _,
    } = release;
    let mut curl = stdx::process::command("curl", Environment::Inherited);
    curl.args(["--location", "--silent", "--show-error", "--fail"])
        .args(["--proto", "=https", "--tlsv1.2", "--output"])
        .arg(to)
        .arg(url);
    ran(&mut curl, step(STEP_TIMEOUT)).map_err(|error| {
        format!(
            "{error}: fetch {name} once online, or fetch every tool before going offline with \
             `cargo xtask install-tools`"
        )
    })?;
    let bytes =
        fs::read(to).map_err(|error| format!("reading {} failed: {error}", to.display()))?;
    let found = self::sha256(&bytes);
    if found != sha256 {
        return Err(format!(
            "{url} has the SHA-256 {found}, not the pinned {sha256}: the download is not \
             {name} {version}"
        )
        .into());
    }
    Ok(())
}

/// The SHA-256 of `bytes`, in lowercase hex.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    let mut hex = String::new();
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Unpacks `archive` into `dir` and then removes the archive. The archive is a zip if its name
/// ends in `.zip`, and a tar otherwise.
pub(crate) fn unpack(archive: &Path, dir: &Path) -> Result<()> {
    let mut unpack = if archive.extension() == Some(OsStr::new("zip")) {
        // Info-ZIP's unzip has no long options: `-q` keeps it quiet, and `-d` names the directory.
        let mut unzip = stdx::process::command("unzip", Environment::Inherited);
        unzip.arg("-q").arg(archive).arg("-d").arg(dir);
        unzip
    } else {
        let mut tar = stdx::process::command("tar", Environment::Inherited);
        tar.args(["--extract", "--file"])
            .arg(archive)
            .arg("--directory")
            .arg(dir);
        tar
    };
    ran(&mut unpack, step(STEP_TIMEOUT))?;
    fs::remove_file(archive)
        .map_err(|error| format!("removing {} failed: {error}", archive.display()).into())
}
