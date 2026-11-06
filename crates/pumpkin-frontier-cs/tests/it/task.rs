//! Taking a ported task from its directory through [`Task::load`]: the task a directory's name
//! gives, and the directories the factory refuses, for their names or their recipes.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fs;

use expect_test::{Expect, expect};
use pumpkin_frontier_cs::Task;

/// A recipe the factory accepts.
const SCRATCH: &str = "FROM scratch\n";

/// A recipe shaped as LWE's agent's: comments, continued lines, and words that Docker would
/// expand or unquote in instructions that fetch nothing.
const LWE: &str = "# The shared base, pinned by digest: apt set to one dated snapshot.\n\
    FROM docker.io/joyemang33/frontier-agent-base:v6@<digest>\n\
    \n\
    ARG DEBIAN_FRONTEND=noninteractive\n\
    RUN packages='git python3' \\\n    \
        && retry() { for attempt in $(seq 30); do \"$@\" && return; done; return 1; } \\\n    \
        && retry apt-get update --error-on=any\n\
    LABEL org.opencontainers.image.title=\"FrontierCS2 task lwe\" \\\n      \
        dev.frontier.problem-id=\"lwe\"\n\
    COPY repo /frontier/task/repo\n\
    USER 1000:1000\n\
    ENV HOME=/workspace TMPDIR=/workspace\n\
    WORKDIR /workspace\n";

/// A recipe of several stages, which name earlier ones by name, in any case, and by index.
const STAGES: &str = "ARG VERSION=1\n\
    FROM --platform=linux/amd64 docker.io/library/ubuntu:24.04@<digest> AS Task\n\
    # syntax=docker/dockerfile:1, a comment once an instruction is before it\n\
    FROM scratch\n\
    COPY --from=Task / /\n\
    COPY --from=0 --chmod=0444 /etc/hostname /\n\
    COPY --from=scratch / /\n\
    from task AS again\n";

/// A recipe whose continued lines join as written, across a comment and a blank line, with a
/// carriage return before each line feed.
const JOINED: &str = "FROM docker.io/library/ubuntu@sha256:0123456789abcdef0123456789abcdef\\\r\n\
    # a comment\r\n\
    \r\n\
    0123456789abcdef0123456789abcdef AS base\r\n\
    RUN true \\\r\n\
    \r\n\
    # FROM ubuntu, as a comment\r\n \
    FROM ubuntu, as part of the command\r\n";

/// The tasks [`recipes`] loads, each a path, its agent's recipe and its verifier's.
const RECIPES: [(&str, &str, &str); 25] = [
    ("lwe", LWE, SCRATCH),
    ("stages", STAGES, SCRATCH),
    ("joined", JOINED, SCRATCH),
    (
        "directive",
        "#!/usr/bin/env build\n# syntax=docker/dockerfile:1\nFROM scratch\n",
        SCRATCH,
    ),
    // With `` ` `` as the escape character, Docker ends the `RUN` at its `\`, and reads a `FROM`.
    (
        "escape",
        "# escape=`\nFROM scratch\nRUN echo \\\nFROM ubuntu\n",
        SCRATCH,
    ),
    // Docker drops a byte-order mark before the first line, and then reads the directive.
    (
        "bom",
        "\u{feff}# syntax=docker/dockerfile:1\nFROM scratch\n",
        SCRATCH,
    ),
    (
        "slashes",
        "// syntax=docker/dockerfile:1\nFROM scratch\n",
        SCRATCH,
    ),
    ("unpinned", "FROM ubuntu:24.04\n", SCRATCH),
    (
        "variable",
        "ARG BASE=docker.io/library/ubuntu@<digest>\nFROM ${BASE}\n",
        SCRATCH,
    ),
    ("quoted", "FROM 'scratch'\n", SCRATCH),
    ("shape", "FROM scratch base\n", SCRATCH),
    (
        "later",
        "FROM base\nFROM docker.io/library/ubuntu@<digest> AS base\n",
        SCRATCH,
    ),
    (
        "copy",
        "FROM scratch\nCOPY --from=ubuntu:24.04 / /\n",
        SCRATCH,
    ),
    // Docker takes a number too large to be an index for an image's name.
    (
        "index",
        "FROM scratch\nCOPY --from=9223372036854775808 / /\n",
        SCRATCH,
    ),
    (
        "quoted-flag",
        "FROM scratch\nCOPY --from=\"ubuntu:24.04 x\" / /\n",
        SCRATCH,
    ),
    (
        "add",
        "FROM scratch\nADD https://example.com/notes /notes\n",
        SCRATCH,
    ),
    // Docker takes what follows a no-break space for a source, even if it starts with `--`.
    (
        "add-git",
        "FROM scratch\nADD \u{a0}--x@example.com:notes /notes\n",
        SCRATCH,
    ),
    // `ADD` fetches each of its sources that is a URL.
    (
        "add-second",
        "FROM scratch\nADD notes https://example.com/notes /notes\n",
        SCRATCH,
    ),
    (
        "mount",
        "FROM scratch\nRUN --mount=from=ubuntu:24.04,target=/ubuntu true\n",
        SCRATCH,
    ),
    // Docker unescapes a flag, so it reads this one as `--mount`.
    (
        "mount-escaped",
        "FROM scratch\nRUN --mo\\unt=from=ubuntu:24.04,target=/ubuntu true\n",
        SCRATCH,
    ),
    (
        "heredoc",
        "FROM scratch\nRUN <<EOF\nFROM ubuntu\nEOF\n",
        SCRATCH,
    ),
    // Docker ends this heredoc at `LABEL\`, which then continues no line, and reads a `FROM`.
    (
        "heredoc-copy",
        "FROM scratch\nCOPY <<'LABEL\\' /x\nENV notes=1\nLABEL\\\n FROM ubuntu\n",
        SCRATCH,
    ),
    (
        "onbuild",
        "FROM scratch\nONBUILD ADD https://example.com/notes /notes\n",
        SCRATCH,
    ),
    // A `\` after another doesn't continue its line.
    (
        "escaped",
        "FROM scratch\nRUN echo \\\\\nFROM ubuntu\n",
        SCRATCH,
    ),
    ("verifier", SCRATCH, "FROM ubuntu\n"),
];

/// Makes, in a scratch directory of the test named `test`, each of `tasks`: a directory at a
/// path below it, with its agent's and its verifier's recipes, in which `@<digest>` stands for
/// a digest; loads each, and compares what each gives, the task or why the factory refuses it,
/// with `expect`.
fn check(test: &str, tasks: &[(&str, &str, &str)], expect: &Expect) {
    let scratch =
        std::env::temp_dir().join(format!("pumpkin-frontier-cs-{}-{test}", std::process::id()));
    let digest = format!("@sha256:{}", "0123456789abcdef".repeat(4));
    let outcomes: Vec<String> = (tasks.iter())
        .map(|&(path, agent, verifier)| {
            let dir = scratch.join(path);
            for (recipe, text) in [
                ("agent-base/Dockerfile", agent),
                ("verifier/Dockerfile", verifier),
            ] {
                let file = dir.join(recipe);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, text.replace("@<digest>", &digest)).unwrap();
            }
            match Task::load(&dir) {
                Ok(task) => format!("{path}: {task:?}"),
                Err(error) => format!("{path}: {error}"),
            }
        })
        .collect();
    fs::remove_dir_all(&scratch).unwrap();
    let outcomes = outcomes.join("\n");
    expect.assert_eq(&outcomes.replace(&scratch.display().to_string(), "<scratch>"));
}

/// A task's problem id is its directory's name, read from the path alone, and a path whose last
/// name isn't a problem id is refused.
#[test]
fn load() {
    let tasks = ["tasks/frontier-cs/tiny", "tiny/", "LWE", "tiny/.."];
    check(
        "load",
        &tasks.map(|path| (path, SCRATCH, SCRATCH)),
        &expect![[r#"
            tasks/frontier-cs/tiny: Task { dir: "<scratch>/tasks/frontier-cs/tiny", problem_id: Name("tiny") }
            tiny/: Task { dir: "<scratch>/tiny/", problem_id: Name("tiny") }
            LWE: <scratch>/LWE: the directory's name, the task's problem id: `LWE` is not a valid name: use 1 to 64 characters from a-z, 0-9, `_` and `-`, starting with a letter
            tiny/..: `<scratch>/tiny/..` doesn't end in a directory's name: give the task's directory by a path that ends in its name, the task's problem id"#]],
    );
}

/// A recipe is read as Docker reads it, and refused if it may have Docker fetch an image no
/// digest pins, a frontend or a URL, or if it holds what the check can't read as Docker would.
#[test]
fn recipes() {
    check(
        "recipes",
        &RECIPES,
        &expect![[r#"
            lwe: Task { dir: "<scratch>/lwe", problem_id: Name("lwe") }
            stages: Task { dir: "<scratch>/stages", problem_id: Name("stages") }
            joined: Task { dir: "<scratch>/joined", problem_id: Name("joined") }
            directive: <scratch>/directive/agent-base/Dockerfile:2: `# syntax=docker/dockerfile:1` reads as a parser directive, which can name a frontend for the build to fetch, or change how Docker reads the lines after it: drop it, or reword the comment
            escape: <scratch>/escape/agent-base/Dockerfile:1: `# escape=`` reads as a parser directive, which can name a frontend for the build to fetch, or change how Docker reads the lines after it: drop it, or reword the comment
            bom: <scratch>/bom/agent-base/Dockerfile:1: `\u{feff}#` is no Dockerfile instruction: start each instruction with one, such as `FROM`, `RUN` or `COPY`
            slashes: <scratch>/slashes/agent-base/Dockerfile:1: `//` is no Dockerfile instruction: start each instruction with one, such as `FROM`, `RUN` or `COPY`
            unpinned: <scratch>/unpinned/agent-base/Dockerfile:1: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch` or an earlier stage
            variable: <scratch>/variable/agent-base/Dockerfile:2: `${BASE}` names a variable, which Docker expands and the check doesn't: write its value instead
            quoted: <scratch>/quoted/agent-base/Dockerfile:1: `'scratch'` holds `'`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            shape: <scratch>/shape/agent-base/Dockerfile:1: `scratch base` isn't an image, then optionally `AS` and a stage's name: write `FROM <image>` or `FROM <image> AS <name>`
            later: <scratch>/later/agent-base/Dockerfile:1: `base` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch` or an earlier stage
            copy: <scratch>/copy/agent-base/Dockerfile:2: `ubuntu:24.04` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch` or an earlier stage
            index: <scratch>/index/agent-base/Dockerfile:2: `9223372036854775808` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch` or an earlier stage
            quoted-flag: <scratch>/quoted-flag/agent-base/Dockerfile:2: `--from="ubuntu:24.04` holds `"`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            add: <scratch>/add/agent-base/Dockerfile:2: `https://example.com/notes` may be a URL, for `ADD` to fetch: copy files from the task's directory with `COPY`
            add-git: <scratch>/add-git/agent-base/Dockerfile:2: `--x@example.com:notes` may be a URL, for `ADD` to fetch: copy files from the task's directory with `COPY`
            add-second: <scratch>/add-second/agent-base/Dockerfile:2: `https://example.com/notes` may be a URL, for `ADD` to fetch: copy files from the task's directory with `COPY`
            mount: <scratch>/mount/agent-base/Dockerfile:2: `RUN --mount` can mount an image no digest pins, or what the host keeps: copy what the step needs with `COPY` instead
            mount-escaped: <scratch>/mount-escaped/agent-base/Dockerfile:2: `--mo\unt=from=ubuntu:24.04,target=/ubuntu` holds `\`, which Docker may split, unquote or unescape: write it in printable ASCII, with no quote or `\`
            heredoc: <scratch>/heredoc/agent-base/Dockerfile:2: this `RUN` holds `<<`, which may begin a heredoc, and the check reads none: put the file in the task's directory and copy it from there
            heredoc-copy: <scratch>/heredoc-copy/agent-base/Dockerfile:2: this `COPY` holds `<<`, which may begin a heredoc, and the check reads none: put the file in the task's directory and copy it from there
            onbuild: <scratch>/onbuild/agent-base/Dockerfile:2: `ONBUILD` runs its instruction in a later build, which the check doesn't see: drop it
            escaped: <scratch>/escaped/agent-base/Dockerfile:3: `ubuntu` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch` or an earlier stage
            verifier: <scratch>/verifier/verifier/Dockerfile:1: `ubuntu` isn't pinned by digest: name it as `<image>@sha256:<64 hexadecimal digits>`, `scratch` or an earlier stage"#]],
    );
}
