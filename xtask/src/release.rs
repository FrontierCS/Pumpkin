//! A release contains the static install, its commit and the formatter's license. Publication
//! attaches the checked archive and its checksum to a draft before making the release public.

use std::fs;
use std::path::Path;

use stdx::Context as _;
use xshell::{Shell, cmd};

use crate::{Cargo, MUSL, Result, install, root};

pub(super) fn run(cargo: &Cargo, tag: &str, publish: bool) -> Result<()> {
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
    let output = root().join("target/release-assets");
    let archive = format!("pumpkin-{tag}-{MUSL}.tar.xz");
    if publish {
        let tag_ref = format!("refs/tags/{tag}^{{commit}}");
        let commit = cmd!(sh, "git rev-parse --verify {tag_ref}").read()?;
        sh.change_dir(output);
        return publish_assets(&sh, tag, &archive, &commit);
    }

    let commit = cmd!(sh, "git rev-parse HEAD").read()?;
    sh.create_dir(&output)?;
    sh.change_dir(&output);
    // Stage beside the outputs so a checked archive can replace a previous one by rename.
    let scratch = cmd!(sh, "mktemp --directory .build.XXXXXXXX").read()?;
    sh.change_dir(scratch);
    let built = package(cargo, &sh, &output, &archive, &commit);
    let cleaned = sh.remove_path(sh.current_dir());
    built?;
    Ok(cleaned?)
}

fn package(cargo: &Cargo, sh: &Shell, output: &Path, archive: &str, commit: &str) -> Result<()> {
    install(cargo, Some(sh.current_dir().join("dist")), MUSL)?;
    sh.write_file("dist/commit", format!("{commit}\n"))?;
    sh.copy_file(
        root().join("deploy/buildifier-LICENSE"),
        "dist/buildifier-LICENSE",
    )?;
    // The upstream formatter includes compressed Go debug sections in its release binary.
    cmd!(sh, "strip --strip-debug dist/bin/buildifier").run()?;

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
    sh.write_file(&checksum, checksum_line(sh, archive)?)?;
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

fn checksum_line(sh: &Shell, archive: &str) -> Result<String> {
    let sum = cmd!(sh, "sha256sum {archive}").read()?;
    Ok(format!("{sum}\n"))
}

fn publish_assets(sh: &Shell, tag: &str, archive: &str, commit: &str) -> Result<()> {
    let checksum = format!("{archive}.sha256");
    if sh.read_file(&checksum)? != checksum_line(sh, archive)? {
        return Err(format!("{checksum} differs from the checksum of {archive}").into());
    }
    let archived = cmd!(sh, "tar --extract --to-stdout --xz --file {archive} commit").read()?;
    if archived != commit {
        return Err(format!(
            "{archive} contains commit {archived:?}, but tag {tag} names {commit}"
        )
        .into());
    }
    cmd!(
        sh,
        "gh release create {tag} --draft --verify-tag --title {tag} --generate-notes"
    )
    .run()?;
    cmd!(sh, "gh release upload {tag} {archive} {checksum}").run()?;
    cmd!(sh, "gh release edit {tag} --draft=false --latest=true").run()?;
    Ok(())
}
