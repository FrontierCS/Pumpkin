//! A release ships a launcher, its one-line pin and the static archive named by the pin.
//! Publication checks all three against the tag before attaching them to a draft.

use std::ffi::OsStr;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use stdx::Context as _;
use stdx::process::{self, Bound, Echo, Environment, Run};

use crate::repo::{Repo, STEP_TIMEOUT};
use crate::{MUSL, Result, build, ran, read, root, step, tools};

struct Release<'a> {
    tag: &'a str,
    commit: String,
    archive: String,
    launcher: Vec<u8>,
}

impl Release<'_> {
    /// The pin of the archive in `dir`.
    fn pin(&self, dir: &Path) -> Result<String> {
        let digest = tools::sha256(&read_file(&dir.join(&self.archive))?);
        Ok(format!("{} {} {digest}\n", self.tag, self.commit))
    }
}

pub(super) fn run(tag: &str, publish: bool) -> Result<()> {
    if !tag.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(format!(
            "unsupported release tag {tag:?}: use letters, digits, dots, underscores and hyphens, \
             starting with a letter or digit"
        )
        .into());
    }

    let repo = Repo { root: root() };
    let revision = if publish {
        format!("refs/tags/{tag}^{{commit}}")
    } else {
        "HEAD".to_owned()
    };
    let commit = repo.git(["rev-parse", "--verify", &revision])?;
    let mut show = repo.command(["show", &format!("{commit}:dist/pumpkin")]);
    let launcher = captured(&mut show)?;
    let release = Release {
        tag,
        commit,
        archive: format!("pumpkin-{tag}-{MUSL}.tar.xz"),
        launcher,
    };

    let output = root().join("target/release-assets").join(tag);
    if publish {
        return publish_assets(&output, &release);
    }

    fs::create_dir_all(&output).context(format_args!("making {} failed", output.display()))?;
    // Staged beside the outputs, so that a checked archive replaces a previous one by a rename.
    let staging = tempfile::Builder::new()
        .prefix(".build.")
        .tempdir_in(&output);
    let staging = staging.context(format_args!(
        "making a directory in {} failed",
        output.display()
    ))?;
    let built = package(staging.path(), &output, &release);
    let cleaned = staging
        .close()
        .context("removing the staging directory failed");
    built?;
    Ok(cleaned?)
}

/// Builds the archive and the launcher in `dir` and checks them. Then it moves them and their
/// pin to `output`.
fn package(dir: &Path, output: &Path, release: &Release<'_>) -> Result<()> {
    let archive = &release.archive;
    let commit = &release.commit;
    let dist = dir.join("dist");
    build(Some(dist.clone()), MUSL)?;
    write(&dist.join("commit"), format!("{commit}\n"))?;
    let license = root().join("dist/buildifier-LICENSE");
    fs::copy(&license, dist.join("buildifier-LICENSE"))
        .context(format_args!("copying {} failed", license.display()))?;
    // The upstream formatter includes compressed Go debug sections in its release binary.
    let mut strip = program("strip", dir);
    ran(
        strip.args(["--strip-debug", "dist/bin/buildifier"]),
        step(STEP_TIMEOUT),
    )?;

    let mut tar = program("tar", dir);
    tar.args(["--create", "--file", archive, "--directory", "dist"])
        .args([
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
        ])
        .arg("--mode=u=rwX,go=rX")
        .args(["--use-compress-program", "xz --threads=1"])
        .args(["bin", "buildifier-LICENSE", "commit"]);
    ran(&mut tar, step(STEP_TIMEOUT))?;
    let unpacked = dir.join("unpacked");
    fs::create_dir(&unpacked).context(format_args!("making {} failed", unpacked.display()))?;
    let mut tar = program("tar", dir);
    tar.args([
        "--extract",
        "--xz",
        "--file",
        archive,
        "--directory",
        "unpacked",
    ]);
    ran(&mut tar, step(STEP_TIMEOUT))?;
    check(&unpacked, commit)?;

    let launcher = dir.join("pumpkin");
    write(&launcher, &release.launcher)?;
    // Through std, not `chmod`, as TigerBeetle's scripts make a file executable:
    // https://github.com/tigerbeetle/tigerbeetle/blob/3cd7cf4799f65c81ffbc36eda788b8b26fd7da72/src/shell.zig#L251-L258
    fs::set_permissions(&launcher, Permissions::from_mode(0o755)).context(format_args!(
        "making {} executable failed",
        launcher.display()
    ))?;
    write(&dir.join("pumpkinversion"), release.pin(dir)?)?;
    for name in ["pumpkin", "pumpkinversion", archive] {
        let (from, to) = (dir.join(name), output.join(name));
        fs::rename(&from, &to).context(format_args!(
            "moving {} to {} failed",
            from.display(),
            to.display()
        ))?;
    }
    Ok(())
}

/// Fails unless the archive unpacked at `dir` names `commit`. Its programs must be static x86-64
/// executables that run.
fn check(dir: &Path, commit: &str) -> Result<()> {
    if read_text(&dir.join("commit"))? != format!("{commit}\n") {
        return Err(format!("archived commit differs from the checkout's {commit}").into());
    }
    for file in install::File::ALL {
        let name = file.name();
        if !file.is_executable() {
            let path = dir.join("bin").join(name);
            if fs::metadata(&path).context(path.display())?.len() == 0 {
                return Err(format!("archived bin/{name} is empty").into());
            }
            continue;
        }
        let mut described = program("file", dir);
        described
            .args(["--brief", &format!("bin/{name}")])
            .env("LC_ALL", "C");
        let elf = read(&mut described, STEP_TIMEOUT)?;
        if !(elf.starts_with("ELF 64-bit")
            && elf.contains("x86-64")
            && ["statically linked", "static-pie linked", "static linked"]
                .iter()
                .any(|kind| elf.contains(kind)))
        {
            return Err(format!("bin/{name} is {elf}: expected a static x86-64 ELF").into());
        }
    }
    let mut buildifier = program(dir.join("bin/buildifier"), dir);
    ran(buildifier.arg("--version"), step(STEP_TIMEOUT))?;
    let mut pumpkin = program(dir.join("bin/pumpkin"), dir);
    let version = read(pumpkin.arg("--version"), STEP_TIMEOUT)?;
    if !version.contains(&format!("({commit:.9} ")) {
        return Err(
            format!("archived pumpkin reports {version:?}: expected commit {commit}").into(),
        );
    }
    println!("{version}");
    Ok(())
}

fn publish_assets(dir: &Path, release: &Release<'_>) -> Result<()> {
    let Release {
        tag,
        commit,
        archive,
        launcher,
    } = release;
    if read_file(&dir.join("pumpkin"))? != *launcher {
        return Err(format!("pumpkin differs from dist/pumpkin at tag {tag}").into());
    }
    if read_text(&dir.join("pumpkinversion"))? != release.pin(dir)? {
        return Err(
            format!("pumpkinversion differs from the pin for tag {tag} and {archive}").into(),
        );
    }
    let mut tar = program("tar", dir);
    tar.args([
        "--extract",
        "--to-stdout",
        "--xz",
        "--file",
        archive,
        "commit",
    ]);
    if captured(&mut tar)? != format!("{commit}\n").as_bytes() {
        return Err(format!("the commit in {archive} differs from tag {tag} ({commit})").into());
    }
    let mut create = program("gh", dir);
    create.args([
        "release",
        "create",
        tag,
        "--draft",
        "--verify-tag",
        "--title",
        tag,
    ]);
    ran(create.arg("--generate-notes"), step(STEP_TIMEOUT))?;
    let mut upload = program("gh", dir);
    upload.args([
        "release",
        "upload",
        tag,
        "pumpkin",
        "pumpkinversion",
        archive,
    ]);
    ran(&mut upload, step(STEP_TIMEOUT))?;
    let mut edit = program("gh", dir);
    edit.args(["release", "edit", tag, "--draft=false", "--latest=true"]);
    ran(&mut edit, step(STEP_TIMEOUT))?;
    Ok(())
}

/// `program`, run in `dir`.
fn program(program: impl AsRef<OsStr>, dir: &Path) -> Command {
    let mut command = process::command(program, Environment::Inherited);
    command.current_dir(dir);
    command
}

/// What `command` printed, as it printed it.
fn captured(command: &mut Command) -> Result<Vec<u8>> {
    let how = Run::new(Bound::Within(STEP_TIMEOUT), Echo::Silent);
    Ok(ran(command, how)?.stdout)
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    Ok(fs::read(path).context(format_args!("reading {} failed", path.display()))?)
}

fn read_text(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path).context(format_args!("reading {} failed", path.display()))?)
}

fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    Ok(fs::write(path, contents).context(format_args!("writing {} failed", path.display()))?)
}
