//! The recipe check: the recipes it reads as Docker does and passes, and those it refuses, for
//! what they may have Docker fetch or for what it can't read as Docker would, with what their
//! builds are given.

use std::collections::BTreeMap;

use expect_test::{Expect, expect};
use recipe::{Given, Stage};

/// Checks each of `recipes`, a name and a text in which `<sha256>` stands for a digest, as the
/// recipe of that name of a build given `given`, in whose args `<sha256>` stands for it too, and
/// compares with `expect` what each gives: that it passes, or why the check refuses it.
fn check(recipes: &[(&str, &str)], given: &Given<()>, expect: &Expect) {
    let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
    let args = (given.args.iter())
        .map(|(name, value)| (name.clone(), value.replace("<sha256>", &digest)))
        .collect();
    let given = Given {
        target: given.target.clone(),
        args,
        contexts: given.contexts.clone(),
    };
    let outcomes: Vec<String> = (recipes.iter())
        .map(|&(name, text)| {
            let text = text.replace("<sha256>", &digest);
            match recipe::check(name, text.as_bytes(), &given) {
                Ok(()) => format!("{name}: passes"),
                Err(error) => error,
            }
        })
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
}

const AGENT: &str = "# The shared base, pinned by digest: apt set to one dated snapshot.\n\
    FROM docker.io/example/agent-base:v6@<sha256>\n\
    \n\
    ARG DEBIAN_FRONTEND=noninteractive\n\
    RUN packages='git python3' \\\n    \
        && retry() { for attempt in $(seq 30); do \"$@\" && return; done; return 1; } \\\n    \
        && retry apt-get update --error-on=any\n\
    LABEL org.opencontainers.image.title=\"a task\" \\\n      \
        dev.example.task=\"task\"\n\
    COPY repo /task/repo\n\
    USER 1000:1000\n\
    ENV HOME=/workspace TMPDIR=/workspace\n\
    WORKDIR /workspace\n";

const STAGES: &str = "ARG VERSION=1\n\
    FROM --platform=linux/amd64 docker.io/library/ubuntu:24.04@<sha256> AS Task\n\
    # syntax=docker/dockerfile:1, a comment once an instruction is before it\n\
    FROM scratch\n\
    COPY --from=Task / /\n\
    COPY --from=0 --chmod=0444 /etc/hostname /\n\
    COPY --from=scratch / /\n\
    from task AS again\n";

const JOINED: &str = "FROM docker.io/library/ubuntu@sha256:0123456789abcdef0123456789abcdef\\\r\n\
    # a comment\r\n\
    \r\n\
    0123456789abcdef0123456789abcdef AS base\r\n\
    RUN true \\\r\n\
    \r\n\
    # FROM ubuntu, as a comment\r\n \
    FROM ubuntu, as part of the command\r\n";

#[test]
fn framing() {
    check(
        &[
            // Comments, continued lines, and words that Docker would expand or unquote in
            // instructions that fetch nothing.
            ("agent", AGENT),
            // Continued lines joined as written, across a comment and a blank line, with a
            // carriage return before each line feed.
            ("joined", JOINED),
            (
                "directive",
                "#!/usr/bin/env build\n# syntax=docker/dockerfile:1\nFROM scratch\n",
            ),
            // With `` ` `` as the escape character, Docker ends the `RUN` at its `\`, and reads
            // a `FROM`.
            (
                "escape",
                "# escape=`\nFROM scratch\nRUN echo \\\nFROM ubuntu\n",
            ),
            // Docker drops a byte-order mark before the first line, and then reads the directive.
            (
                "bom",
                "\u{feff}# syntax=docker/dockerfile:1\nFROM scratch\n",
            ),
            ("slashes", "// syntax=docker/dockerfile:1\nFROM scratch\n"),
            ("heredoc", "FROM scratch\nRUN <<EOF\nFROM ubuntu\nEOF\n"),
            // Docker ends this heredoc at `LABEL\`, which then continues no line, and reads a
            // `FROM`.
            (
                "heredoc-copy",
                "FROM scratch\nCOPY <<'LABEL\\' /x\nENV notes=1\nLABEL\\\n FROM ubuntu\n",
            ),
            (
                "onbuild",
                "FROM scratch\nONBUILD ADD https://example.com/notes /notes\n",
            ),
            // A `\` after another doesn't continue its line.
            ("escaped", "FROM scratch\nRUN echo \\\\\nFROM ubuntu\n"),
        ],
        &Given::default(),
        &expect![[r#"
            agent: passes
            joined: passes
            directive:2: `# syntax=docker/dockerfile:1` reads as a parser directive, which can name a frontend for the build to fetch, or change how Docker reads the lines after it: drop it, or reword the comment
            escape:1: `# escape=`` reads as a parser directive, which can name a frontend for the build to fetch, or change how Docker reads the lines after it: drop it, or reword the comment
            bom:1: `\u{feff}#` is no Dockerfile instruction: start each instruction with one, such as `FROM`, `RUN` or `COPY`
            slashes:1: `//` is no Dockerfile instruction: start each instruction with one, such as `FROM`, `RUN` or `COPY`
            heredoc:2: this `RUN` holds `<<`, which may begin a heredoc, and the check reads none: put the file in the build's context and copy it from there
            heredoc-copy:2: this `COPY` holds `<<`, which may begin a heredoc, and the check reads none: put the file in the build's context and copy it from there
            onbuild:2: `ONBUILD` runs its instruction in a later build, which the check doesn't see: drop it
            escaped:3: `ubuntu` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given"#]],
    );
}

#[test]
fn images() {
    check(
        &[
            // Stages that name earlier ones by name, in any case, and by index.
            ("stages", STAGES),
            ("base", "FROM pumpkin/task/agent-base:0123\n"),
            ("unpinned", "FROM ubuntu:24.04\n"),
            ("short-digest", "FROM ubuntu@sha256:0123\n"),
            (
                "variable",
                "ARG BASE=docker.io/library/ubuntu@<sha256>\nFROM ${BASE}\n",
            ),
            ("quoted", "FROM 'scratch'\n"),
            ("shape", "FROM scratch TO base\n"),
            (
                "later",
                "FROM base\nFROM docker.io/library/ubuntu@<sha256> AS base\n",
            ),
            ("copy", "FROM scratch\nCOPY --from=ubuntu:24.04 / /\n"),
            (
                "copy-later",
                "FROM scratch\nCOPY --from=1 / /\nFROM scratch\n",
            ),
            // Docker takes a number too large to be an index for an image's name.
            (
                "index",
                "FROM scratch\nCOPY --from=9223372036854775808 / /\n",
            ),
            (
                "quoted-flag",
                "FROM scratch\nCOPY --from=\"ubuntu:24.04 x\" / /\n",
            ),
            (
                "mount",
                "FROM scratch\nRUN --mount=from=ubuntu:24.04,target=/ubuntu true\n",
            ),
            // Docker unescapes a flag, so it reads this one as `--mount`.
            (
                "mount-escaped",
                "FROM scratch\nRUN --mo\\unt=from=ubuntu:24.04,target=/ubuntu true\n",
            ),
        ],
        &Given::default(),
        &expect![[r#"
            stages: passes
            base:1: `pumpkin/task/agent-base:0123` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            unpinned:1: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            short-digest:1: `ubuntu@sha256:0123` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            variable:2: `${BASE}` names the build argument `BASE`, which the build isn't given: give it an image pinned by digest
            quoted:1: `'scratch'` holds `'`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            shape:1: `scratch TO base` isn't an image, then optionally `AS` and a stage's name: write `FROM <image>` or `FROM <image> AS <name>`
            later:1: `base` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            copy:2: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            copy-later:2: `1` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            index:2: `9223372036854775808` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            quoted-flag:2: `--from="ubuntu:24.04` holds `"`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            mount:2: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            mount-escaped:2: `--mo\unt=from=ubuntu:24.04,target=/ubuntu` holds `\`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`"#]],
    );
}

#[test]
fn sources() {
    check(
        &[
            ("add-local", "FROM scratch\nADD notes/v1.0 /notes\n"),
            (
                "add",
                "FROM scratch\nADD https://example.com/notes /notes\n",
            ),
            // Docker takes what follows a no-break space for a source, even if it starts with
            // `--`.
            (
                "add-git",
                "FROM scratch\nADD \u{a0}--x@example.com:notes /notes\n",
            ),
            // `ADD` fetches each of its sources that is a URL.
            (
                "add-second",
                "FROM scratch\nADD notes https://example.com/notes /notes\n",
            ),
            (
                "checksum",
                "FROM scratch\nADD --checksum=<sha256> https://example.com/notes.tgz /notes\n",
            ),
            (
                "checksum-two",
                "FROM scratch\nADD --checksum=<sha256> https://example.com/a https://example.com/b \
                 /notes/\n",
            ),
            (
                "checksum-git",
                "FROM scratch\nADD --checksum=<sha256> https://example.com/notes.git /notes\n",
            ),
            // Docker may read a source with no scheme as an `https://` URL.
            (
                "add-repository",
                "FROM scratch\nADD example.com/notes.GIT /notes\n",
            ),
            // Docker may read `%2E` in a URL's path as `.`, and end the path at `?` or `#`.
            (
                "checksum-escaped",
                "FROM scratch\nADD --checksum=<sha256> https://example.com/notes%2Egit /notes\n",
            ),
            (
                "checksum-query",
                "FROM scratch\nADD --checksum=<sha256> https://example.com/notes.git?x /notes\n",
            ),
            (
                "checksum-fragment",
                "FROM scratch\nADD --checksum=<sha256> https://example.com/notes.git#main /notes\n",
            ),
            (
                "checksum-http",
                "FROM scratch\nADD --checksum=<sha256> http://example.com/notes /notes\n",
            ),
            (
                "checksum-short",
                "FROM scratch\nADD --checksum=sha256:0123 https://example.com/notes /notes\n",
            ),
        ],
        &Given::default(),
        &expect![[r#"
            add-local: passes
            add:2: `https://example.com/notes` may be a URL, for `ADD` to fetch, that no checksum pins: copy files from the build's context with `COPY`, or fetch one `https://` URL with `ADD --checksum=sha256:<64 hexadecimal digits>`
            add-git:2: `--x@example.com:notes` may be a URL, for `ADD` to fetch, that no checksum pins: copy files from the build's context with `COPY`, or fetch one `https://` URL with `ADD --checksum=sha256:<64 hexadecimal digits>`
            add-second:2: `https://example.com/notes` may be a URL, for `ADD` to fetch, that no checksum pins: copy files from the build's context with `COPY`, or fetch one `https://` URL with `ADD --checksum=sha256:<64 hexadecimal digits>`
            checksum: passes
            checksum-two:2: `https://example.com/a` may be a URL, for `ADD` to fetch, that no checksum pins: copy files from the build's context with `COPY`, or fetch one `https://` URL with `ADD --checksum=sha256:<64 hexadecimal digits>`
            checksum-git:2: Docker may take `https://example.com/notes.git` for a Git repository's URL, which some of its versions clone without checking a checksum: name no `ADD` source that ends in `.git`, in any case
            add-repository:2: Docker may take `example.com/notes.GIT` for a Git repository's URL, which some of its versions clone without checking a checksum: name no `ADD` source that ends in `.git`, in any case
            checksum-escaped:2: `https://example.com/notes%2Egit` holds `%`, which can hide a `.git` ending: name a plain `ADD` source, with no escape, query or fragment, so no `%`, `?` or `#`
            checksum-query:2: `https://example.com/notes.git?x` holds `?`, which can hide a `.git` ending: name a plain `ADD` source, with no escape, query or fragment, so no `%`, `?` or `#`
            checksum-fragment:2: `https://example.com/notes.git#main` holds `#`, which can hide a `.git` ending: name a plain `ADD` source, with no escape, query or fragment, so no `%`, `?` or `#`
            checksum-http:2: `http://example.com/notes` may be a URL, for `ADD` to fetch, that no checksum pins: copy files from the build's context with `COPY`, or fetch one `https://` URL with `ADD --checksum=sha256:<64 hexadecimal digits>`
            checksum-short:2: `sha256:0123` isn't a SHA-256 digest: write `--checksum=sha256:<64 hexadecimal digits>`"#]],
    );
}

#[test]
fn steps() {
    check(
        &[
            (
                "mount",
                "FROM scratch\nRUN --mount=from=ubuntu:24.04,target=/ubuntu true\n",
            ),
            // A bind mount, Docker's default, of the build's context, an earlier stage, in any case,
            // or an image pinned by digest.
            (
                "mount-bind",
                "FROM docker.io/library/ubuntu@<sha256> AS tools\nFROM scratch\n\
             RUN --mount=target=/src --mount=type=BIND,source=file,target=/file \
             --mount=from=Tools,target=/tools --mount=from=docker.io/library/ubuntu@<sha256>,ro \
             true\n",
            ),
            // Docker takes a stage's index here for an image's name.
            (
                "mount-index",
                "FROM scratch\nFROM scratch\nRUN --mount=from=0,target=/x true\n",
            ),
            (
                "mount-cache",
                "FROM scratch\nRUN --mount=target=/src --mount=type=cache,target=/root/.cache true\n",
            ),
            (
                "mount-secret",
                "FROM scratch\nRUN --mount=type=Secret,id=key true\n",
            ),
            // A later field of a key sets it again, its key in any case.
            (
                "mount-again",
                "FROM scratch\nRUN --mount=type=bind,TYPE=ssh true\n",
            ),
            (
                "mount-tmpfs",
                "FROM scratch\nRUN --mount=type=tmpfs,target=/t true\n",
            ),
            (
                "mount-volume",
                "FROM scratch\nRUN --mount=type=volume,target=/v true\n",
            ),
            (
                "mount-bare",
                "FROM scratch\nRUN --mount=type,target=/v true\n",
            ),
            // Docker expands a mount's values.
            (
                "mount-variable",
                "FROM scratch\nARG KIND=cache\nRUN --mount=type=$KIND,target=/c true\n",
            ),
        ],
        &Given::default(),
        &expect![[r#"
            mount:2: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            mount-bind: passes
            mount-index:3: a `RUN --mount`'s `from=0` names the image `docker.io/library/0`, as Docker reads a number there, not a stage's index: name the stage by its `AS` name
            mount-cache:2: `RUN --mount=type=cache` mounts what the host keeps, which the build's inputs don't name: mount the build's context, an earlier stage, a named context or an image pinned by digest with `type=bind`, or copy what the step needs with `COPY`
            mount-secret:2: `RUN --mount=type=secret` mounts what the host keeps, which the build's inputs don't name: mount the build's context, an earlier stage, a named context or an image pinned by digest with `type=bind`, or copy what the step needs with `COPY`
            mount-again:2: `RUN --mount=type=ssh` mounts what the host keeps, which the build's inputs don't name: mount the build's context, an earlier stage, a named context or an image pinned by digest with `type=bind`, or copy what the step needs with `COPY`
            mount-tmpfs: passes
            mount-volume:2: `volume` is no type of mount that the check passes: mount with `type=bind` or `type=tmpfs`
            mount-bare:2: a `RUN --mount`'s `type` is empty: give it `bind` or `tmpfs`, the types of mount that the check passes
            mount-variable:3: `type=$KIND,target=/c` names a variable, which Docker expands and the check doesn't: write its value instead"#]],
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "most of it is the cases and their snapshot"
)]
#[expect(
    clippy::zero_sized_map_values,
    reason = "a check builds no image, so it is given its named contexts by their names alone, as `Given<()>`"
)]
fn given() {
    let given = |target: &str, args: &[(&str, &str)]| Given {
        target: Some(Stage::in_any_case(target).unwrap()),
        args: (args.iter())
            .map(|&(name, value)| (name.to_owned(), value.to_owned()))
            .collect::<BTreeMap<_, _>>(),
        contexts: BTreeMap::from([("build-tools".to_owned(), ()), ("toolchain".to_owned(), ())]),
    };
    let args = [
        ("BASE", "docker.io/library/ubuntu@<sha256>"),
        ("TAGGED", "busybox:1.37"),
        ("STAGE", "train"),
        ("QUOTED", "'scratch'"),
    ];
    let recipes = [
        ("braced", "ARG BASE TAGGED\nFROM ${BASE} AS train\n"),
        ("bare", "ARG TAGGED BASE=x\nFROM $BASE AS Train\n"),
        (
            "stage",
            "ARG STAGE\nFROM build-tools AS train\nFROM ${STAGE}\n",
        ),
        (
            "contexts",
            "FROM build-tools AS train\nCOPY --from=toolchain /usr/local/go /usr/local/go\n",
        ),
        ("not given", "FROM scratch AS train\nCOPY --from=go / /\n"),
        (
            "stage named as one",
            "FROM scratch AS train\nFROM scratch AS Toolchain\n",
        ),
        ("tagged", "ARG TAGGED\nFROM ${TAGGED} AS train\n"),
        ("quoted", "ARG QUOTED\nFROM ${QUOTED} AS train\n"),
        ("undeclared", "FROM ${BASE} AS train\n"),
        (
            "in-stage",
            "FROM scratch AS train\nARG BASE\nFROM ${BASE}\n",
        ),
        (
            "absent",
            "ARG ABSENT=docker.io/library/ubuntu@<sha256>\nFROM ${ABSENT} AS train\n",
        ),
        ("default", "ARG BASE\nFROM ${BASE:-scratch} AS train\n"),
        (
            "within",
            "ARG BASE\nFROM docker.io/library/ubuntu@${BASE} AS train\n",
        ),
        (
            "copy",
            "ARG BASE\nFROM scratch AS train\nCOPY --from=${BASE} / /\n",
        ),
        (
            "mount",
            "FROM scratch AS train\nRUN --mount=from=toolchain,target=/usr/local/go true\n",
        ),
        ("target", "FROM scratch AS eval\nFROM scratch\n"),
    ];
    check(
        &recipes,
        &given("train", &args),
        &expect![[r#"
            braced: passes
            bare: passes
            stage: passes
            contexts: passes
            not given:2: `go` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            stage named as one:2: the stage `toolchain` is named as a named context the build is given, which Docker would build in its place: name the stage otherwise
            tagged:2: `${TAGGED}` expands to the build argument's value: `busybox:1.37` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a named context the build is given
            quoted:2: `${QUOTED}` expands to the build argument's value: `'scratch'` holds `'`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            undeclared:1: `${BASE}` names `BASE`, which no `ARG` before the first `FROM` declares, so Docker would expand it to nothing or to a value of its own: declare it there with `ARG BASE`, and give the build its value
            in-stage:3: `${BASE}` names `BASE`, which no `ARG` before the first `FROM` declares, so Docker would expand it to nothing or to a value of its own: declare it there with `ARG BASE`, and give the build its value
            absent:2: `${ABSENT}` names the build argument `ABSENT`, which the build isn't given: give it an image pinned by digest
            default:2: `${BASE:-scratch}` names a variable, which Docker expands and the check doesn't: write its value instead
            within:2: `docker.io/library/ubuntu@${BASE}` names a variable, which Docker expands and the check doesn't: write its value instead
            copy:3: `${BASE}` names a variable, which Docker expands and the check doesn't: write its value instead
            mount: passes
            target: the build's target, `train`, names no stage of the recipe: name one that a `FROM <image> AS <name>` begins"#]],
    );
    let refused = [("buildkit", "FROM scratch AS train\n")];
    check(
        &refused,
        &given("train", &[("BUILDKIT_SYNTAX", "docker/dockerfile:1")]),
        &expect![
            "buildkit: `BUILDKIT_SYNTAX` starts with `BUILDKIT_`, a prefix BuildKit reads for itself, as `BUILDKIT_SYNTAX` names a frontend to fetch: give no build argument with it"
        ],
    );
    check(
        &refused,
        &given("train", &[("1X", "x")]),
        &expect![
            "buildkit: `1X` is no build argument's name: name it with ASCII letters, digits and `_`, not starting with a digit"
        ],
    );
    let capitals = [("capitals", "FROM scratch AS train\n")];
    check(
        &capitals,
        &given("TRAIN", &[]),
        &expect!["capitals: passes"],
    );
    let named = |name: &str| Given {
        contexts: BTreeMap::from([(name.to_owned(), ())]),
        ..Given::default()
    };
    let recipe = [("named", "FROM scratch\n")];
    check(
        &recipe,
        &named("Go"),
        &expect![
            "named: `Go` is no named context's name: name it with words of lowercase ASCII letters and digits, joined each by one `-`, `_` or `.`, starting with a letter"
        ],
    );
    check(
        &recipe,
        &named("go..x"),
        &expect![
            "named: `go..x` is no named context's name: name it with words of lowercase ASCII letters and digits, joined each by one `-`, `_` or `.`, starting with a letter"
        ],
    );
    check(
        &recipe,
        &named("go-"),
        &expect![
            "named: `go-` is no named context's name: name it with words of lowercase ASCII letters and digits, joined each by one `-`, `_` or `.`, starting with a letter"
        ],
    );
    check(
        &recipe,
        &named("1go"),
        &expect![
            "named: `1go` is no named context's name: name it with words of lowercase ASCII letters and digits, joined each by one `-`, `_` or `.`, starting with a letter"
        ],
    );
    check(
        &recipe,
        &named("scratch"),
        &expect![
            "named: `scratch` is the empty image's name, which no named context may take: name it otherwise"
        ],
    );
}
