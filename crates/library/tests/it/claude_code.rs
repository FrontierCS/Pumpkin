//! `@pumpkin//harness/claude_code/claude_code.star`'s `claude_code()`: the agent role it gives
//! for each set of arguments, and the arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use lang::Args;
use schema::ir;

use super::{check, evaluated, layer, route, show};

/// A task's entry that gives `claude_code()` each of its arguments from an `--arg` of its name,
/// and builds its agent on the image of `//images/Dockerfile`, which the judge runs on too.
const ENTRY: &str = r#"
load("@pumpkin//harness/claude_code/claude_code.star", "claude_code")

MODEL = arg("model", str)
EFFORT = arg("effort", str)
WORKSPACE = arg("workspace", str)
PROMPT = arg("prompt", str)
DEADLINE = arg("deadline_seconds", int)
GRACE = arg("finalize_grace_seconds", int)
MAX_OUTPUT_TOKENS = arg("max_output_tokens", int)

BASE = image("Dockerfile", context = "//images")

agent = claude_code(
    BASE,
    model = MODEL,
    effort = EFFORT,
    workspace = WORKSPACE,
    prompt = PROMPT,
    deadline_seconds = DEADLINE,
    finalize_grace_seconds = GRACE,
    max_output_tokens = MAX_OUTPUT_TOKENS,
)

workflow(
    name = "claude-code",
    roles = [
        {
            "name": "agent",
            "image": agent["image"],
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 1000, "memory_bytes": GiB},
            "runtime": agent["runtime"],
            "egress": agent["egress"],
            "grants": [{"submit": {"name": "final"}}],
        },
        {
            "name": "judge",
            "image": BASE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"judge": {
                "command": {"argv": ["true"], "cwd": "/pumpkin/home"},
                "slots": [{"name": "payload", "max_bytes": KiB}],
                "result": {"score": "/score", "returns": ["/score"]},
            }},
        },
    ],
    score = {"role": "judge", "slot": "payload", "timeout_seconds": 60},
)
"#;

/// The names of `ENTRY`'s arguments, in the order a case gives them.
const NAMES: [&str; 7] = [
    "model",
    "effort",
    "workspace",
    "prompt",
    "deadline_seconds",
    "finalize_grace_seconds",
    "max_output_tokens",
];

/// The arguments of `case`, by their names, as `ENTRY` takes them.
fn args(case: &[&str; 7]) -> Args {
    let given: Vec<String> = (NAMES.iter().zip(case))
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    Args::parse(given.iter().map(String::as_str)).unwrap()
}

/// A prompt of the task repository's form, shortened.
const FCS_PROMPT: &str = "Solve the evaluation task in /frontier/task/README.md.

When the answer is ready, run `/.pumpkin/bin/sandboxctl submit final <tar> --finalize`.
";

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    let mut cases = vec![
        [
            "claude-opus-5-5",
            "max",
            "/workspace",
            FCS_PROMPT,
            "18000",
            "60",
            "64000",
        ],
        [
            "glm-5.3",
            "max",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
            "128000",
        ],
        [
            r#"vendor/"quoted"\model"#,
            "high",
            r#"/srv/my "work" dir"#,
            "-leading dash\nsecond line, \"quoted\"\n",
            "60",
            "0",
            "1",
        ],
        ["model-small", "high", "/", "Go.", "60", "0", "32000"],
    ];
    for effort in ["low", "medium", "xhigh"] {
        cases.push([
            "model-small",
            effort,
            "/workspace",
            "Go.",
            "60",
            "0",
            "32000",
        ]);
    }
    let mut shown = String::new();
    for (index, case) in cases.iter().enumerate() {
        let (workflow, images) = evaluated(ENTRY, &args(case)).unwrap();
        let [model, effort, workspace, prompt, deadline, grace, max] = case;
        let agent = &workflow.ir().roles[0];
        let ir::Runtime::Agent(runtime) = &agent.runtime else {
            panic!("the first role is the agent");
        };
        assert_eq!(runtime.stdin.as_deref(), Some(*prompt));
        assert_eq!(runtime.deadline_seconds.to_string(), *deadline);
        assert_eq!(runtime.finalize_grace_seconds.to_string(), *grace);
        assert!(
            runtime.files.is_empty(),
            "Claude Code reads no file of ours"
        );
        let _ = writeln!(shown, "{model} {effort} {workspace} {max}:");
        shown.push_str(&show(runtime, |_| String::new()));
        shown.push_str(&route(agent));
        if index == 0 {
            shown.push_str(&layer(agent, images));
        }
    }
    check(
        &shown,
        &expect![[r#"
            claude-opus-5-5 max /workspace 64000:
            argv: /harness/claude-code-2.1.296/claude --print --model claude-opus-5-5 --effort max --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: HOME=/workspace/.claude-code CLAUDE_CONFIG_DIR=/workspace/.claude-code/config TMPDIR=/workspace/.claude-code/tmp ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=claude-opus-5-5 ANTHROPIC_DEFAULT_MODEL=claude-opus-5-5 ANTHROPIC_SMALL_FAST_MODEL=claude-opus-5-5 ANTHROPIC_DEFAULT_FABLE_MODEL=claude-opus-5-5 ANTHROPIC_DEFAULT_OPUS_MODEL=claude-opus-5-5 ANTHROPIC_DEFAULT_SONNET_MODEL=claude-opus-5-5 ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-opus-5-5 CLAUDE_CODE_SUBAGENT_MODEL=claude-opus-5-5 CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=claude-opus-5-5 DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=64000
            cwd: /workspace
            dir 0700 /workspace/.claude-code
            dir 0700 /workspace/.claude-code/config
            dir 0700 /workspace/.claude-code/tmp
            route: messages claude-opus-5-5
            request headers: anthropic-beta user-agent x-app
            response headers: none
            image: @pumpkin//harness/claude_code/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/claude_code/layer
              FROM debian@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS claude-code-2.1.296
              ADD --checksum=sha256:eef5a2e2b09a5e7d2ce784d1b3ba335ae7fc6c7f6256c802a0ebf2c21e4fd0e1 https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-/claude-code-linux-x64-2.1.296.tgz /harness.tgz
              ADD --checksum=sha256:38861b171079e440b25ac86483284f690d0ee7921dba98b0c8fe04a9b0a11667 https://github.com/NixOS/patchelf/releases/download/0.19.3/patchelf-0.19.3-x86_64.tar.gz /patchelf.tgz
              RUN tar --extract --gzip --file /harness.tgz --directory / package/claude package/LICENSE.md && mkdir /patchelf && tar --extract --gzip --file /patchelf.tgz --directory /patchelf ./bin/patchelf && /patchelf/bin/patchelf --set-interpreter /harness/claude-code-2.1.296/lib/ld-linux-x86-64.so.2 --set-rpath /harness/claude-code-2.1.296/lib /package/claude && mkdir --parents /out/harness/claude-code-2.1.296/lib && mv /package/claude /package/LICENSE.md /out/harness/claude-code-2.1.296/ && cd /lib/x86_64-linux-gnu && cp --dereference ld-linux-x86-64.so.2 libc.so.6 libdl.so.2 libm.so.6 libpthread.so.0 librt.so.1 /out/harness/claude-code-2.1.296/lib/ && cd /out/harness/claude-code-2.1.296 && chmod 0444 LICENSE.md lib/libc.so.6 lib/libdl.so.2 lib/libm.so.6 lib/libpthread.so.0 lib/librt.so.1 && chmod 0555 /out/harness . lib claude lib/ld-linux-x86-64.so.2
              FROM base
              COPY --link --from=claude-code-2.1.296 --chown=0:0 /out/ /
            glm-5.3 max /workspace 128000:
            argv: /harness/claude-code-2.1.296/claude --print --model glm-5.3 --effort max --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: HOME=/workspace/.claude-code CLAUDE_CONFIG_DIR=/workspace/.claude-code/config TMPDIR=/workspace/.claude-code/tmp ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=glm-5.3 ANTHROPIC_DEFAULT_MODEL=glm-5.3 ANTHROPIC_SMALL_FAST_MODEL=glm-5.3 ANTHROPIC_DEFAULT_FABLE_MODEL=glm-5.3 ANTHROPIC_DEFAULT_OPUS_MODEL=glm-5.3 ANTHROPIC_DEFAULT_SONNET_MODEL=glm-5.3 ANTHROPIC_DEFAULT_HAIKU_MODEL=glm-5.3 CLAUDE_CODE_SUBAGENT_MODEL=glm-5.3 CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=glm-5.3 DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=128000
            cwd: /workspace
            dir 0700 /workspace/.claude-code
            dir 0700 /workspace/.claude-code/config
            dir 0700 /workspace/.claude-code/tmp
            route: messages glm-5.3
            vendor/"quoted"\model high /srv/my "work" dir 1:
            argv: /harness/claude-code-2.1.296/claude --print --model vendor/"quoted"\model --effort high --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: "HOME=/srv/my \"work\" dir/.claude-code" "CLAUDE_CONFIG_DIR=/srv/my \"work\" dir/.claude-code/config" "TMPDIR=/srv/my \"work\" dir/.claude-code/tmp" ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=vendor/"quoted"\model ANTHROPIC_DEFAULT_MODEL=vendor/"quoted"\model ANTHROPIC_SMALL_FAST_MODEL=vendor/"quoted"\model ANTHROPIC_DEFAULT_FABLE_MODEL=vendor/"quoted"\model ANTHROPIC_DEFAULT_OPUS_MODEL=vendor/"quoted"\model ANTHROPIC_DEFAULT_SONNET_MODEL=vendor/"quoted"\model ANTHROPIC_DEFAULT_HAIKU_MODEL=vendor/"quoted"\model CLAUDE_CODE_SUBAGENT_MODEL=vendor/"quoted"\model CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=vendor/"quoted"\model DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=1
            cwd: /srv/my "work" dir
            dir 0700 /srv/my "work" dir/.claude-code
            dir 0700 /srv/my "work" dir/.claude-code/config
            dir 0700 /srv/my "work" dir/.claude-code/tmp
            route: messages vendor/"quoted"\model
            model-small high / 32000:
            argv: /harness/claude-code-2.1.296/claude --print --model model-small --effort high --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: HOME=/.claude-code CLAUDE_CONFIG_DIR=/.claude-code/config TMPDIR=/.claude-code/tmp ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=model-small ANTHROPIC_DEFAULT_MODEL=model-small ANTHROPIC_SMALL_FAST_MODEL=model-small ANTHROPIC_DEFAULT_FABLE_MODEL=model-small ANTHROPIC_DEFAULT_OPUS_MODEL=model-small ANTHROPIC_DEFAULT_SONNET_MODEL=model-small ANTHROPIC_DEFAULT_HAIKU_MODEL=model-small CLAUDE_CODE_SUBAGENT_MODEL=model-small CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=model-small DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=32000
            cwd: /
            dir 0700 /.claude-code
            dir 0700 /.claude-code/config
            dir 0700 /.claude-code/tmp
            route: messages model-small
            model-small low /workspace 32000:
            argv: /harness/claude-code-2.1.296/claude --print --model model-small --effort low --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: HOME=/workspace/.claude-code CLAUDE_CONFIG_DIR=/workspace/.claude-code/config TMPDIR=/workspace/.claude-code/tmp ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=model-small ANTHROPIC_DEFAULT_MODEL=model-small ANTHROPIC_SMALL_FAST_MODEL=model-small ANTHROPIC_DEFAULT_FABLE_MODEL=model-small ANTHROPIC_DEFAULT_OPUS_MODEL=model-small ANTHROPIC_DEFAULT_SONNET_MODEL=model-small ANTHROPIC_DEFAULT_HAIKU_MODEL=model-small CLAUDE_CODE_SUBAGENT_MODEL=model-small CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=model-small DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=32000
            cwd: /workspace
            dir 0700 /workspace/.claude-code
            dir 0700 /workspace/.claude-code/config
            dir 0700 /workspace/.claude-code/tmp
            route: messages model-small
            model-small medium /workspace 32000:
            argv: /harness/claude-code-2.1.296/claude --print --model model-small --effort medium --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: HOME=/workspace/.claude-code CLAUDE_CONFIG_DIR=/workspace/.claude-code/config TMPDIR=/workspace/.claude-code/tmp ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=model-small ANTHROPIC_DEFAULT_MODEL=model-small ANTHROPIC_SMALL_FAST_MODEL=model-small ANTHROPIC_DEFAULT_FABLE_MODEL=model-small ANTHROPIC_DEFAULT_OPUS_MODEL=model-small ANTHROPIC_DEFAULT_SONNET_MODEL=model-small ANTHROPIC_DEFAULT_HAIKU_MODEL=model-small CLAUDE_CODE_SUBAGENT_MODEL=model-small CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=model-small DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=32000
            cwd: /workspace
            dir 0700 /workspace/.claude-code
            dir 0700 /workspace/.claude-code/config
            dir 0700 /workspace/.claude-code/tmp
            route: messages model-small
            model-small xhigh /workspace 32000:
            argv: /harness/claude-code-2.1.296/claude --print --model model-small --effort xhigh --output-format text --dangerously-skip-permissions --disallowedTools WebSearch,WebFetch --strict-mcp-config --mcp-config {"mcpServers":{}} --setting-sources ""
            env: HOME=/workspace/.claude-code CLAUDE_CONFIG_DIR=/workspace/.claude-code/config TMPDIR=/workspace/.claude-code/tmp ANTHROPIC_BASE_URL=http://127.0.0.1:11235 ANTHROPIC_API_KEY=pumpkin ANTHROPIC_MODEL=model-small ANTHROPIC_DEFAULT_MODEL=model-small ANTHROPIC_SMALL_FAST_MODEL=model-small ANTHROPIC_DEFAULT_FABLE_MODEL=model-small ANTHROPIC_DEFAULT_OPUS_MODEL=model-small ANTHROPIC_DEFAULT_SONNET_MODEL=model-small ANTHROPIC_DEFAULT_HAIKU_MODEL=model-small CLAUDE_CODE_SUBAGENT_MODEL=model-small CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL=model-small DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 CLAUDE_CODE_MAX_OUTPUT_TOKENS=32000
            cwd: /workspace
            dir 0700 /workspace/.claude-code
            dir 0700 /workspace/.claude-code/config
            dir 0700 /workspace/.claude-code/tmp
            route: messages model-small
        "#]],
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn refusals() {
    let given = [
        "model-small",
        "high",
        "/workspace",
        "Go.",
        "60",
        "0",
        "32000",
    ];
    let mut refused = Vec::new();
    for (effort, workspace, prompt, max) in [
        ("extreme", "/workspace", "Go.", "32000"),
        ("High", "/workspace", "Go.", "32000"),
        ("high", "/pumpkin/home", "Go.", "32000"),
        ("high", "work", "Go.", "32000"),
        ("high", "/workspace", "", "32000"),
        ("high", "/workspace", "Go.", "0"),
    ] {
        let case = [given[0], effort, workspace, prompt, given[4], given[5], max];
        refused.push(evaluated(ENTRY, &args(&case)).unwrap_err());
    }
    // Each argument given `None`, as a task's `arg(…, default = None)` gives it when no one
    // passes one, and given a value of another type.
    for (argument, value) in [
        ("BASE,", "None,"),
        ("model = MODEL,", "model = None,"),
        ("effort = EFFORT,", "effort = None,"),
        ("workspace = WORKSPACE,", "workspace = None,"),
        ("prompt = PROMPT,", "prompt = None,"),
        ("deadline_seconds = DEADLINE,", "deadline_seconds = None,"),
        (
            "finalize_grace_seconds = GRACE,",
            "finalize_grace_seconds = None,",
        ),
        (
            "max_output_tokens = MAX_OUTPUT_TOKENS,",
            "max_output_tokens = None,",
        ),
        ("model = MODEL,", "model = 5,"),
        ("prompt = PROMPT,", "prompt = [PROMPT],"),
        ("deadline_seconds = DEADLINE,", "deadline_seconds = \"60\","),
        (
            "max_output_tokens = MAX_OUTPUT_TOKENS,",
            "max_output_tokens = \"128000\",",
        ),
    ] {
        let entry = ENTRY.replacen(argument, value, 1);
        refused.push(evaluated(&entry, &args(&given)).unwrap_err());
    }
    check(
        &refused.join("\n"),
        &expect![[r#"
            @pumpkin//harness/claude_code/claude_code.star:77:9: fail: `extreme` is not a reasoning effort: use one of low, medium, high, xhigh, max
                77 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:77:9: fail: `High` is not a reasoning effort: use one of low, medium, high, xhigh, max
                77 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:85:13: `/pumpkin/home/.claude-code` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                85 |     home = ("" if workspace == "/" else workspace) + "/.claude-code"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            task.star:6:13: `work` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
                6 | WORKSPACE = arg("workspace", str)
                  |             ^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/command/cwd
            @pumpkin//harness/claude_code/claude_code.star:79:9: fail: Claude Code runs only on a prompt: give a string that is not empty
                79 |         fail("Claude Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:81:9: fail: `0` is not a number of tokens: give a positive int
                81 |         fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:14:9
            task.star:15:5: a value of `image`'s `contexts` can't be `None`: give an image, as `image()` makes
                15 |     None,
                   |     ^^^^
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:97:21: `+` can't take a string and `None`: give it two ints, two strings, two lists or two tuples
                97 |     ] + [name + "=" + model for name in _MODEL_VARIABLES] + [
                   |                     ^
                the left operand, `"ANTHROPIC_MODEL="`, is made at @pumpkin//harness/claude_code/claude_code.star:97:10
                the right operand, `None`, is made at task.star:16:13
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:77:69: `format` of `None` is not in Pumpkin's Starlark: give it an int or a string
                77 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |                                                                     ^^^^^^
                the argument, `None`, is made at task.star:17:14
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:85:52: `+` can't take `None` and a string: give it two ints, two strings, two lists or two tuples
                85 |     home = ("" if workspace == "/" else workspace) + "/.claude-code"
                   |                                                    ^
                the left operand, `None`, is made at task.star:18:17
                the right operand, `"/.claude-code"`, is made at @pumpkin//harness/claude_code/claude_code.star:85:54
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:79:9: fail: Claude Code runs only on a prompt: give a string that is not empty
                79 |         fail("Claude Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:14:9
            task.star:20:24: `deadline_seconds` can't be `None`: expected an int
                20 |     deadline_seconds = None,
                   |                        ^^^^
                the IR's /roles/0/runtime/agent/deadline_seconds
            task.star:21:30: `finalize_grace_seconds` can't be `None`: expected an int
                21 |     finalize_grace_seconds = None,
                   |                              ^^^^
                the IR's /roles/0/runtime/agent/finalize_grace_seconds
            @pumpkin//harness/claude_code/claude_code.star:80:26: `<` can't take `None` and an int: give it two ints or two strings
                80 |     if max_output_tokens < 1:
                   |                          ^
                the left operand, `None`, is made at task.star:22:25
                the right operand, `1`, is made at @pumpkin//harness/claude_code/claude_code.star:80:28
                in `claude_code()`, called at task.star:14:9
            @pumpkin//harness/claude_code/claude_code.star:97:21: `+` can't take a string and an int: give it two ints, two strings, two lists or two tuples
                97 |     ] + [name + "=" + model for name in _MODEL_VARIABLES] + [
                   |                     ^
                the left operand, `"ANTHROPIC_MODEL="`, is made at @pumpkin//harness/claude_code/claude_code.star:97:10
                the right operand, `5`, is made at task.star:16:13
                in `claude_code()`, called at task.star:14:9
            task.star:19:14: `stdin` can't be a list: expected a string
                19 |     prompt = [PROMPT],
                   |              ^^^^^^^^
                the IR's /roles/0/runtime/agent/stdin
            task.star:20:24: `deadline_seconds` can't be a string: expected an int
                20 |     deadline_seconds = "60",
                   |                        ^^^^
                the IR's /roles/0/runtime/agent/deadline_seconds
            @pumpkin//harness/claude_code/claude_code.star:80:26: `<` can't take a string and an int: give it two ints or two strings
                80 |     if max_output_tokens < 1:
                   |                          ^
                the left operand, `"128000"`, is made at task.star:22:25
                the right operand, `1`, is made at @pumpkin//harness/claude_code/claude_code.star:80:28
                in `claude_code()`, called at task.star:14:9"#]],
    );
}
