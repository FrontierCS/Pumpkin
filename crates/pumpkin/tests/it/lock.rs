//! `lock`: a task checked as `check` checks it, and its recipe's fetch stage read, before the
//! configuration and the engine are reached; then the stage written afresh, in place, from what
//! its resolver prints, run online on the engine in the image of the recipe up to the step that
//! reads it, built with the inputs that the task's images give the recipe.

use std::fs;

use expect_test::expect;
use test_support::docker_context;

use super::build::Labelled;
use super::{Scratch, Step, check, machine, ran, workflow};

/// A base of Ubuntu 24.04, with CA certificates, which apt needs for an `https://` archive.
const NOBLE: &str = "docker.io/library/buildpack-deps@sha256:\
    a115b80bacdfad86050664accdf7e2d7be45edb87d98a3ba78b78f0f9a089c1a";

/// The sources of one dated snapshot of Ubuntu's archive, whose files never change: two suites,
/// so that `less=590-2ubuntu2`, `noble`'s, isn't the candidate, `noble-updates`' being newer.
const SOURCES: &str = "Types: deb\n\
    URIs: https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/\n\
    Suites: noble noble-updates\n\
    Components: main\n\
    Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n";

/// A recipe that copies [`SOURCES`] from its context onto the named context `noble`, then
/// whose fetch stage `apt` holds what apt installs of `hello`, of `noble`, and `less`, of a
/// version that isn't the candidate, with the indexes it reads; and whose last step installs
/// them from the stage alone, as a mirror of the archive.
fn recipe() -> String {
    let packages = "hello/noble less=590-2ubuntu2";
    let list = "deb [signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] \
        file:/fetch/apt noble noble-updates main";
    format!(
        "FROM noble AS sourced\n\
         COPY sources /etc/apt/sources.list.d/ubuntu.sources\n\
         \n\
         # What apt installs of the packages, and the indexes it reads.\n\
         # pumpkin lock: apt {packages}\n\
         FROM scratch AS apt\n\
         \n\
         FROM sourced\n\
         RUN --mount=type=bind,from=apt,target=/fetch/apt \\\n    \
             printf '%s\\n' {list:?} > /tmp/mirror.list \\\n    \
             && apt() {{ apt-get --option Dir::Etc::SourceList=/tmp/mirror.list \\\n        \
             -o Dir::Etc::SourceParts=/dev/null -o APT::Sandbox::User=root \"$@\"; }} \\\n    \
             && apt update --error-on=any \\\n    \
             && apt install --yes --no-install-recommends {packages} \\\n    \
             && hello && less --version\n"
    )
}

/// Writes into `scratch` the task `bench`, whose roles' image `recipe` builds from `bench/`,
/// which holds [`SOURCES`] too, given as `noble` the image of `bench/base/Dockerfile`, Ubuntu
/// 24.04 labelled `label`.
fn write_bench(scratch: &Scratch, recipe: &str, label: &str) {
    scratch.write_task("bench", &workflow("", &["true"]));
    let task = fs::read_to_string(scratch.0.join("bench/task.star")).unwrap();
    let images = "NOBLE = image(\"base/Dockerfile\")\n\
        IMAGE = image(\"Dockerfile\", contexts = {\"noble\": NOBLE})\n";
    scratch.write(
        "bench/task.star",
        &task.replace("IMAGE = image(\"Dockerfile\")\n", images),
    );
    scratch.write(
        "bench/base/Dockerfile",
        &format!("FROM {NOBLE}\nLABEL {label}\n"),
    );
    scratch.write("bench/Dockerfile", recipe);
    scratch.write("bench/sources", SOURCES);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn refusals() {
    let scratch = Scratch::new();
    let debs = recipe().replace("AS apt", "AS debs");
    write_bench(
        &scratch,
        &debs.replace("from=apt", "from=debs"),
        "pumpkin.test=lock",
    );
    // A task whose two images give the recipe different args.
    scratch.write_task("pair", &workflow("", &["true"]));
    scratch.write(
        "pair/Dockerfile",
        &recipe().replace("noble AS", "scratch AS"),
    );
    let task = fs::read_to_string(scratch.0.join("pair/task.star")).unwrap();
    let other = "OTHER = image(\"Dockerfile\", args = {\"MODE\": \"other\"})\n";
    scratch.write(
        "pair/task.star",
        &task.replacen("IMAGE = ", &format!("{other}IMAGE = "), 1),
    );
    // A `docker` that says whether it ran.
    fs::create_dir(scratch.0.join("bin")).unwrap();
    scratch.write_executable("bin/docker", b"#!/bin/sh\ntouch \"$0.ran\"\nexit 1\n");
    let alone = |args: &[&str]| {
        let mut command = scratch.pumpkin(&["lock"]);
        command.args(args).env("PATH", scratch.0.join("bin"));
        command
    };
    let steps = [
        scratch.pumpkin(&["lock", "--help"]),
        alone(&["bench/task.star", "bench/Dockerfile", "apt"]),
        alone(&["bench/task.star", "bench/sources", "apt"]),
        alone(&["pair/task.star", "pair/Dockerfile", "apt"]),
        alone(&["bench/task.star", "bench/Dockerfile", "Apt"]),
        alone(&["bench/task.star", "bench/Dockerfile"]),
    ];
    let ran: Vec<String> = (steps.into_iter())
        .map(|command| ran(command, &scratch.0, false))
        .collect();
    let docker = scratch.0.join("bin/docker.ran").exists();
    let ran = Step::Ran(format!("{}\ndocker was run: {docker}", ran.join("\n")));
    check(
        &scratch,
        [ran],
        false,
        &[],
        &expect![[r#"
            $ pumpkin lock --help
            exit status: Some(0)
            printed:
              Write the fetch stage <stage> of the recipe <recipe> of a task afresh, from its
              lock line, `# pumpkin lock: <resolver> <argument>...`, in the comment right
              above its line `FROM scratch AS <stage>`, up to the blank line that ends it: an
              `ADD` for each file that the resolver prints, pinned by its SHA-256. The
              resolver runs online, in the image of the recipe up to the one later `RUN` that
              mounts the stage, built on the machine's Docker engine with the inputs that the
              task's images give the recipe: its context, mounts, args and named contexts,
              which are built first. `apt <package>...` pins what apt fetches to install the
              packages, and the archive's indexes that apt reads, so that the stage is a
              mirror of that part of the archive.

              Usage: pumpkin lock [OPTIONS] <entry> <recipe> <stage>

              Arguments:
                <entry>
                        The task's entry file, as `pumpkin check` takes it.

                <recipe>
                        The recipe, a file that an image() call of the task builds, by its path;
                        the lock rewrites it in place.

                <stage>
                        The fetch stage's name.

              Options:
                    --arg <name>=<value>
                        The value of the arg that an arg() call declares as <name>, as
                        `pumpkin check` takes it.

                    --config <file>
                        The machine's configuration, whose [docker] names the engine; see
                        `pumpkin help config`.
                        Default: $HOME/.config/pumpkin/pumpkin.toml

                -h, --help
                        Print this help.

              Examples:
                pumpkin lock tasks/bench/task.star tasks/bench/verifier/Dockerfile apt
                pumpkin lock tasks/bench/task.star tasks/bench/agent/Dockerfile apt --arg model=m

              Exit status:
                0  The stage is written.
                1  The task has problems; its images give the recipe different inputs; the
                   recipe has no such fetch stage; or a build, the resolver or the write
                   failed; or another error.
                2  A usage error.
              Every exit but 0 ends with a `pumpkin:` line on the standard error, which says
              why. The task is checked as `pumpkin check` checks it, and the stage read,
              before the configuration is read and the engine reached. The builds' output
              goes to the standard error. The image the resolver runs in is tagged in
              pumpkin/lock, where a later lock finds it.
            $ PATH=<scratch>/bin pumpkin lock bench/task.star bench/Dockerfile apt
            exit status: Some(1)
            pumpkin: bench/Dockerfile: no line is `FROM scratch AS apt`: begin the fetch stage with it, below a comment that holds its lock line, `# pumpkin lock: <resolver> <argument>...`
            $ PATH=<scratch>/bin pumpkin lock bench/task.star bench/sources apt
            exit status: Some(1)
            pumpkin: bench/task.star: the task declares no image of the recipe `//bench/sources`: name a recipe that an image() call of the task builds
            $ PATH=<scratch>/bin pumpkin lock pair/task.star pair/Dockerfile apt
            exit status: Some(1)
            pumpkin: pair/task.star: the task's images give the recipe `//pair/Dockerfile` different inputs, as its context, mounts, args or named contexts, for which its stage may resolve differently: give each image a recipe of its own, or the same inputs
            $ PATH=<scratch>/bin pumpkin lock bench/task.star bench/Dockerfile Apt
            exit status: Some(2)
            error: invalid value 'Apt' for '<stage>': `Apt` is no stage's name: use a-z, 0-9, `-`, `_` and `.`, starting with a letter
            pumpkin: to read how to use it: pumpkin help lock
            $ PATH=<scratch>/bin pumpkin lock bench/task.star bench/Dockerfile
            exit status: Some(2)
            error: the following required arguments were not provided:
              <stage>

            Usage: pumpkin lock <entry> <recipe> <stage>
            pumpkin: to read how to use it: pumpkin help lock
            docker was run: false"#]],
    );
}

#[test]
#[ignore = "needs Docker and snapshot.ubuntu.com: run cargo xtask live docker"]
fn locks() {
    let scratch = Scratch::new();
    let label = format!("pumpkin.test=lock-{}", std::process::id());
    let _images = Labelled(label.clone());
    write_bench(&scratch, &recipe(), &label);
    scratch.write("pumpkin.toml", &machine(&docker_context()));
    let config = ["--config", "pumpkin.toml"];
    let lock = || {
        let mut lock = scratch.pumpkin(&["lock", "bench/task.star", "bench/Dockerfile", "apt"]);
        lock.args(config);
        lock
    };
    // Without the builds' output, whose times vary.
    let said = |text: String| {
        let lines = text
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty());
        lines.collect::<Vec<_>>().join("\n")
    };
    let first = said(ran(lock(), &scratch.0, true));
    let written = fs::read_to_string(scratch.0.join("bench/Dockerfile")).unwrap();
    let again = said(ran(lock(), &scratch.0, true));
    let same = fs::read_to_string(scratch.0.join("bench/Dockerfile")).unwrap() == written;
    let built = (scratch.pumpkin(&["build", "bench/task.star"]).args(config))
        .output()
        .unwrap();
    let (_, stage) = written.split_once("\n\n").unwrap();
    let (stage, _) = stage.split_once("\n\n").unwrap();
    let ran = Step::Ran(format!(
        "{first}\n{again}\nwritten the same again: {same}\nbuilt: {}\n{stage}",
        built.status
    ));
    check(
        &scratch,
        [ran],
        true,
        &[(&label, "<label>")],
        &expect![[r#"
            $ pumpkin lock bench/task.star bench/Dockerfile apt --config pumpkin.toml
            exit status: Some(0)
            pumpkin: building an image in pumpkin/bench/base
            pumpkin: built an image in pumpkin/bench/base
            $ pumpkin lock bench/task.star bench/Dockerfile apt --config pumpkin.toml
            exit status: Some(0)
            pumpkin: building an image in pumpkin/bench/base
            pumpkin: built an image in pumpkin/bench/base
            written the same again: true
            built: exit status: 0
            # What apt installs of the packages, and the indexes it reads.
            # pumpkin lock: apt hello/noble less=590-2ubuntu2
            FROM scratch AS apt
            ADD --link --chmod=0644 --checksum=sha256:6b1d7bab86063d6f0f3c4271263c5d23302b4bfbec551335f04f0e4bf60843f4 https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/dists/noble-updates/InRelease /dists/noble-updates/InRelease
            ADD --link --chmod=0644 --checksum=sha256:f686437ba6bdd866edf310b20bf9218e550829ae8933c8c2b88fba7beff2f8f3 https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/dists/noble-updates/main/binary-amd64/by-hash/SHA256/f686437ba6bdd866edf310b20bf9218e550829ae8933c8c2b88fba7beff2f8f3 /dists/noble-updates/main/binary-amd64/Packages.gz
            ADD --link --chmod=0644 --checksum=sha256:cdb2f31d809f589719a53c6ad15f255b27569c4059542ada282aaa21b8e164b0 https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/dists/noble/InRelease /dists/noble/InRelease
            ADD --link --chmod=0644 --checksum=sha256:e0d7e4cbb09d2aa7f9e104a1488817417bc9d85f3e5d9a21156a52ec641ae531 https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/dists/noble/main/binary-amd64/by-hash/SHA256/e0d7e4cbb09d2aa7f9e104a1488817417bc9d85f3e5d9a21156a52ec641ae531 /dists/noble/main/binary-amd64/Packages.gz
            ADD --link --chmod=0644 --checksum=sha256:e68cf4365b7aa9c4e2af4af6eee1710d6f967059b7b4af62786e8870d7366333 https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/pool/main/h/hello/hello_2.10-3build1_amd64.deb /pool/main/h/hello/hello_2.10-3build1_amd64.deb
            ADD --link --chmod=0644 --checksum=sha256:e8560662fbf944dd4a4348faa0d8b4652fbc24939e6fe4ac06c0bdf946f9f935 https://snapshot.ubuntu.com/ubuntu/20261103T000000Z/pool/main/l/less/less_590-2ubuntu2_amd64.deb /pool/main/l/less/less_590-2ubuntu2_amd64.deb"#]],
    );
}
