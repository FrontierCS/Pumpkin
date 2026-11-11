//! The recipe check: the recipes it reads as Docker does and passes, and those it refuses, for
//! what they may have Docker fetch or for what it can't read as Docker would.

use expect_test::{Expect, expect};

/// Checks each of `recipes`, a name and a text in which `<sha256>` stands for a digest, as the
/// recipe of that name of a build on [`BASE`], and compares with `expect` what each gives: that
/// it passes, or why the check refuses it.
fn check(recipes: &[(&str, &str)], expect: &Expect) {
    let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
    let outcomes: Vec<String> = (recipes.iter())
        .map(|&(name, text)| {
            let text = text.replace("<sha256>", &digest);
            match recipe::check(name, text.as_bytes(), &[BASE]) {
                Ok(()) => format!("{name}: passes"),
                Err(error) => error,
            }
        })
        .collect();
    expect.assert_eq(&outcomes.join("\n"));
}

const BASE: &str = "pumpkin/task/agent-base:0123";

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
            escaped:3: `ubuntu` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given"#]],
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
            ("other-base", "FROM pumpkin/task/agent-base:4567\n"),
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
        &expect![[r#"
            stages: passes
            base: passes
            unpinned:1: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            short-digest:1: `ubuntu@sha256:0123` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            other-base:1: `pumpkin/task/agent-base:4567` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            variable:2: `${BASE}` names a variable, which Docker expands and the check doesn't: write its value instead
            quoted:1: `'scratch'` holds `'`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            shape:1: `scratch TO base` isn't an image, then optionally `AS` and a stage's name: write `FROM <image>` or `FROM <image> AS <name>`
            later:1: `base` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            copy:2: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            copy-later:2: `1` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            index:2: `9223372036854775808` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch`, an earlier stage or a base the build is given
            quoted-flag:2: `--from="ubuntu:24.04` holds `"`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            mount:2: `RUN --mount` can mount an image no digest pins, or what the host keeps: copy what the step needs with `COPY` instead
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
