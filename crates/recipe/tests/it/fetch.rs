//! A recipe's fetch stage, read without Docker: the recipes whose stage the lock writes, and
//! those it refuses, for a stage, a lock line or a step that reads the stage that isn't as the
//! stage's form says.

use expect_test::{Expect, expect};
use recipe::Stage;
use recipe::fetch::{FetchStage, Pins};

/// A recipe with the fetch stage `apt`, read by one step, as a task's verifier has it.
const VERIFIER: &str = "# The packages, pinned.\n\
    # pumpkin lock: apt build-essential libglpk-dev/noble python3=3.12.3-0ubuntu2\n\
    FROM scratch AS apt\n\
    ADD --link --chmod=0644 --checksum=sha256:<sha256> https://example.com/a /a\n\
    \n\
    FROM docker.io/example/base@sha256:<sha256> AS common\n\
    USER root\n\
    RUN --mount=type=bind,from=apt,target=/fetch/apt \\\n    \
        sh /context/apt_mirror.sh /fetch/apt install --yes build-essential\n";

/// Reads the stage `stage` of each of `recipes`, a name and a text in which `<sha256>` stands
/// for a digest's hexadecimal digits, as the recipe of that name, and compares with
/// `expect` what each gives: that it is read, or why it is refused.
fn check(stage: &str, recipes: &[(&str, &str)], expect: &Expect) {
    let stage = Stage::try_from(stage.to_owned()).unwrap();
    let read = recipes.iter().map(|(name, text)| {
        let text = text.replace("<sha256>", &"0123456789abcdef".repeat(4));
        match FetchStage::read(name, text.as_bytes(), &stage) {
            Ok(_) => format!("{name}: read"),
            Err(error) => error,
        }
    });
    expect.assert_eq(&(read.collect::<Vec<_>>().join("\n") + "\n"));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
fn stages() {
    let variant = |from: &str, to: &str| VERIFIER.replacen(from, to, 1);
    check(
        "apt",
        &[
            ("verifier", VERIFIER),
            (
                "unlocked",
                &variant(
                    "ADD --link --chmod=0644 --checksum=sha256:<sha256> https://example.com/a /a\n",
                    "",
                ),
            ),
            (
                "no-stage",
                &variant("FROM scratch AS apt", "FROM scratch AS debs"),
            ),
            (
                "twice",
                &format!("{VERIFIER}# pumpkin lock: apt git\nFROM scratch AS apt\n"),
            ),
            ("no-lock", &variant("# pumpkin lock: apt", "# lock: apt")),
            (
                "two-locks",
                &variant("# The packages, pinned.", "# pumpkin lock: apt git"),
            ),
            (
                "no-resolver",
                &variant(
                    "apt build-essential libglpk-dev/noble python3=3.12.3-0ubuntu2",
                    "",
                ),
            ),
            ("pip", &variant("lock: apt", "lock: pip")),
            (
                "no-package",
                &variant(
                    " build-essential libglpk-dev/noble python3=3.12.3-0ubuntu2",
                    "",
                ),
            ),
            (
                "option",
                &variant("libglpk-dev/noble", "--allow-downgrades"),
            ),
            (
                "unended",
                &variant("\n\nFROM docker.io", "\nFROM docker.io"),
            ),
            ("unread", &variant("from=apt", "from=common")),
            (
                "read-twice",
                &format!("{VERIFIER}RUN --mount=type=bind,from=APT,target=/a true\n"),
            ),
            (
                "cached",
                &variant("type=bind,from=apt", "type=cache,from=apt"),
            ),
        ],
        &expect![[r#"
            verifier: read
            unlocked: read
            no-stage: no line is `FROM scratch AS apt`: begin the fetch stage with it, below a comment that holds its lock line, `# pumpkin lock: <resolver> <argument>...`
            twice:11: `FROM scratch AS apt` begins a second stage `apt`: name each stage once
            no-lock:3: the stage `apt` has no lock line in the comment right above it: add `# pumpkin lock: <resolver> <argument>...`, as `# pumpkin lock: apt <package>...`
            two-locks:1: the stage `apt` has two lock lines: keep one
            no-resolver:2: the lock line names no resolver: name `apt`
            pip:2: `pip` is no resolver: name `apt`
            no-package:2: the lock line names no package for apt: name each that the step installs
            option:2: `--allow-downgrades` is no package for apt: name each as `<name>`, `<name>=<version>` or `<name>/<release>`
            unended:5: the stage `apt` holds a line that is no `ADD` before the blank line that ends it: end the stage with a blank line, as `pumpkin lock` writes it
            unread:3: no later `RUN` bind-mounts the stage `apt`: mount it, as `RUN --mount=type=bind,from=apt,target=<dir>`, in the step that reads it
            read-twice:10: this `RUN` bind-mounts the stage `apt` too, but its files are resolved for one step: give each step a fetch stage of its own
            cached:3: no later `RUN` bind-mounts the stage `apt`: mount it, as `RUN --mount=type=bind,from=apt,target=<dir>`, in the step that reads it
        "#]],
    );
}

#[test]
fn written() {
    let sha256 = "0123456789abcdef".repeat(4);
    let text = VERIFIER.replace("<sha256>", &sha256);
    let stage = Stage::try_from("apt".to_owned()).unwrap();
    let stage = FetchStage::read("verifier", text.as_bytes(), &stage).unwrap();
    let a = "https://example.com/a";
    let b = "https://example.com/b";
    let hash = sha256.as_str();
    let cases: &[&[(&str, &str, &str)]] = &[
        &[("dir/a", a, hash), ("b", b, hash)],
        &[("a", a, hash), ("a", b, hash)],
        &[("a", a, hash), ("a", a, &"0".repeat(64))],
        &[("a", a, hash), ("a", a, hash)],
        &[],
        &[("a", a, "0123")],
        &[("a", "http://example.com/a", hash)],
        &[("a", "https://example.com/a.git", hash)],
        &[("../a", a, hash)],
        &[("dir/with space", a, hash)],
    ];
    let write = |entries: &[(&str, &str, &str)]| {
        let mut pins = Pins::default();
        for &(path, url, hash) in entries {
            pins.insert(path.to_owned(), url.to_owned(), hash)?;
        }
        stage.written(&pins)
    };
    let written = cases
        .iter()
        .map(|entries| write(entries).unwrap_or_else(|error| error));
    expect![[r#"
        # The packages, pinned.
        # pumpkin lock: apt build-essential libglpk-dev/noble python3=3.12.3-0ubuntu2
        FROM scratch AS apt
        ADD --link --chmod=0644 --checksum=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef https://example.com/b /b
        ADD --link --chmod=0644 --checksum=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef https://example.com/a /dir/a

        FROM docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef AS common
        USER root
        RUN --mount=type=bind,from=apt,target=/fetch/apt \
            sh /context/apt_mirror.sh /fetch/apt install --yes build-essential

        ---
        `a` is pinned twice, to different files: pin each path once
        ---
        `a` is pinned twice, to different files: pin each path once
        ---
        # The packages, pinned.
        # pumpkin lock: apt build-essential libglpk-dev/noble python3=3.12.3-0ubuntu2
        FROM scratch AS apt
        ADD --link --chmod=0644 --checksum=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef https://example.com/a /a

        FROM docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef AS common
        USER root
        RUN --mount=type=bind,from=apt,target=/fetch/apt \
            sh /context/apt_mirror.sh /fetch/apt install --yes build-essential

        ---
        the resolver found no files to pin
        ---
        `0123` isn't a SHA-256: use 64 lowercase hexadecimal digits
        ---
        `http://example.com/a` isn't an `https://` URL: fetch each file by one
        ---
        Docker may take `https://example.com/a.git` for a Git repository's URL, which some of its versions clone without checking a checksum: name no `ADD` source that ends in `.git`, in any case
        ---
        `../a` isn't a relative path with no empty, `.` or `..` component: name one
        ---
        `dir/with space` holds ` `, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`"#]].assert_eq(&written.collect::<Vec<_>>().join("\n---\n"));
}
