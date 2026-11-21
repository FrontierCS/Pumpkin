//! A release contains the static install, its commit and the formatter's license. The tag
//! workflow publishes the archive and checksum once the extracted install passes its checks.

use std::fs;
use std::path::Path;

use stdx::Context as _;
use xshell::{Shell, cmd};

use crate::{Cargo, MUSL, Result, install, root};

pub(super) fn build(cargo: &Cargo, tag: &str) -> Result<()> {
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

    let sh = cargo.sh.clone();
    let commit = cmd!(sh, "git rev-parse HEAD").read()?;
    let output = sh.create_dir("target/release-assets")?;
    sh.change_dir(&output);
    // Stage beside the outputs so a checked archive can replace a previous one by rename.
    let scratch = cmd!(sh, "mktemp --directory .build.XXXXXXXX").read()?;
    sh.change_dir(scratch);
    let built = package(cargo, &sh, &output, tag, &commit);
    let cleaned = sh.remove_path(sh.current_dir());
    built?;
    Ok(cleaned?)
}

fn package(cargo: &Cargo, sh: &Shell, output: &Path, tag: &str, commit: &str) -> Result<()> {
    install(cargo, Some(sh.current_dir().join("dist")), MUSL)?;
    sh.write_file("dist/commit", format!("{commit}\n"))?;
    sh.copy_file(
        root().join("deploy/buildifier-LICENSE"),
        "dist/buildifier-LICENSE",
    )?;
    // The upstream formatter includes compressed Go debug sections in its release binary.
    cmd!(sh, "strip --strip-debug dist/bin/buildifier").run()?;

    let archive = format!("pumpkin-{tag}-{MUSL}.tar.xz");
    cmd!(sh, "tar --create --file {archive} --directory dist")
        .args([
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
        ])
        .arg("--mode=u=rwX,go=rX")
        .args(["--use-compress-program", "xz --threads=1"])
        .args(["bin", "buildifier-LICENSE", "commit"])
        .run()?;
    sh.create_dir("unpacked")?;
    cmd!(
        sh,
        "tar --extract --xz --file {archive} --directory unpacked"
    )
    .run()?;
    check(sh, commit)?;

    let checksum = format!("{archive}.sha256");
    let sum = cmd!(sh, "sha256sum {archive}").read()?;
    sh.write_file(&checksum, format!("{sum}\n"))?;
    cmd!(sh, "mv --target-directory {output} {archive} {checksum}").run()?;
    Ok(())
}

fn check(sh: &Shell, commit: &str) -> Result<()> {
    let _dir = sh.push_dir("unpacked");
    if sh.read_file("commit")? != format!("{commit}\n") {
        return Err(format!("archived commit differs from the checkout's {commit}").into());
    }
    for program in ["pumpkin", "sandboxd", "ec2-boot", "buildifier"] {
        let elf = cmd!(sh, "file --brief bin/{program}")
            .env("LC_ALL", "C")
            .read()?;
        if !(elf.starts_with("ELF 64-bit")
            && elf.contains("x86-64")
            && ["statically linked", "static-pie linked", "static linked"]
                .iter()
                .any(|kind| elf.contains(kind)))
        {
            return Err(format!("bin/{program} is {elf}: expected a static x86-64 ELF").into());
        }
    }
    for helper in ["pumpkin-modal.py", "pumpkin-modal.py.lock"] {
        let path = sh.current_dir().join("bin").join(helper);
        if fs::metadata(&path).context(path.display())?.len() == 0 {
            return Err(format!("archived bin/{helper} is empty").into());
        }
    }
    cmd!(sh, "bin/buildifier --version").run()?;
    let version = cmd!(sh, "bin/pumpkin --version").read()?;
    if !version.contains(&format!("({commit:.9} ")) {
        return Err(
            format!("archived pumpkin reports {version:?}: expected commit {commit}").into(),
        );
    }
    println!("{version}");
    Ok(())
}
