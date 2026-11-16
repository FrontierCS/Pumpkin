//! `@pumpkin//harness/claude_code/claude_code.star`'s `claude_code()`: the agent role it gives
//! for each set of arguments, and the arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;

use super::{FCS_PROMPT, agent, args, check, entry, evaluated, layer, route, show};

/// The arguments `claude_code()` takes besides `SHARED`, each with the Starlark of its value.
const OWN: [(&str, &str); 1] = [("max_output_tokens", r#"arg("max_output_tokens", int)"#)];

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
        let (workflow, images) = evaluated(&entry("claude_code", &OWN), &args(&OWN, case)).unwrap();
        let [model, effort, workspace, prompt, _, _, max] = case;
        let (agent, runtime) = agent(&workflow, case);
        assert_eq!(runtime.stdin.as_deref(), Some(*prompt));
        assert!(
            runtime.files.is_empty(),
            "Claude Code reads no file of ours"
        );
        let _ = writeln!(shown, "{model} {effort} {workspace} {max}:");
        shown.push_str(&show(runtime, |_| String::new()));
        shown.push_str(&route(agent));
        if index == 0 {
            shown.push_str(&layer(agent, &images));
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
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/claude_code/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/claude_code/layer
            the agent's image: image 2
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
        refused.push(evaluated(&entry("claude_code", &OWN), &args(&OWN, &case)).unwrap_err());
    }
    // Each argument given `None`, as a task's `arg(…, default = None)` gives it when no one
    // passes one, and given a value of another type.
    for (argument, value) in [
        ("BASE,", "None,"),
        ("model = MODEL,", "model = None,"),
        ("effort = EFFORT,", "effort = None,"),
        ("workspace = WORKSPACE,", "workspace = None,"),
        ("prompt = PROMPT,", "prompt = None,"),
        (
            "deadline_seconds = DEADLINE_SECONDS,",
            "deadline_seconds = None,",
        ),
        (
            "finalize_grace_seconds = FINALIZE_GRACE_SECONDS,",
            "finalize_grace_seconds = None,",
        ),
        (
            "max_output_tokens = MAX_OUTPUT_TOKENS,",
            "max_output_tokens = None,",
        ),
        ("model = MODEL,", "model = 5,"),
        ("prompt = PROMPT,", "prompt = [PROMPT],"),
        (
            "deadline_seconds = DEADLINE_SECONDS,",
            "deadline_seconds = \"60\",",
        ),
        (
            "max_output_tokens = MAX_OUTPUT_TOKENS,",
            "max_output_tokens = \"128000\",",
        ),
    ] {
        let entry = entry("claude_code", &OWN).replacen(argument, value, 1);
        refused.push(evaluated(&entry, &args(&OWN, &given)).unwrap_err());
    }
    check(
        &refused.join("\n"),
        &expect![[r#"
            @pumpkin//harness/claude_code/claude_code.star:77:9: fail: `extreme` is not a reasoning effort: use one of low, medium, high, xhigh, max
                77 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:77:9: fail: `High` is not a reasoning effort: use one of low, medium, high, xhigh, max
                77 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:85:13: `/pumpkin/home/.claude-code` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                85 |     home = ("" if workspace == "/" else workspace) + "/.claude-code"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            task.star:5:13: `work` is not a normalized absolute path: start with `/`, and use no empty, `.` or `..` components and no NUL byte
                5 | WORKSPACE = arg("workspace", str)
                  |             ^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/command/cwd
            @pumpkin//harness/claude_code/claude_code.star:79:9: fail: Claude Code runs only on a prompt: give a string that is not empty
                79 |         fail("Claude Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:81:9: fail: `0` is not a number of tokens: give a positive int
                81 |         fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:13:9
            task.star:14:5: a value of `image`'s `contexts` can't be `None`: give an image, as `image()` makes
                14 |     None,
                   |     ^^^^
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:97:21: `+` can't take a string and `None`: give it two numbers, two strings, two lists or two tuples
                97 |     ] + [name + "=" + model for name in _MODEL_VARIABLES] + [
                   |                     ^
                the left operand, `"ANTHROPIC_MODEL="`, is made at @pumpkin//harness/claude_code/claude_code.star:97:10
                the right operand, `None`, is made at task.star:15:13
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:77:69: `format` of `None` is not in Pumpkin's Starlark: give it a number or a string
                77 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |                                                                     ^^^^^^
                the argument, `None`, is made at task.star:16:14
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:85:52: `+` can't take `None` and a string: give it two numbers, two strings, two lists or two tuples
                85 |     home = ("" if workspace == "/" else workspace) + "/.claude-code"
                   |                                                    ^
                the left operand, `None`, is made at task.star:17:17
                the right operand, `"/.claude-code"`, is made at @pumpkin//harness/claude_code/claude_code.star:85:54
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:79:9: fail: Claude Code runs only on a prompt: give a string that is not empty
                79 |         fail("Claude Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `claude_code()`, called at task.star:13:9
            task.star:19:24: `deadline_seconds` can't be `None`: expected an int
                19 |     deadline_seconds = None,
                   |                        ^^^^
                the IR's /roles/0/runtime/agent/deadline_seconds
            task.star:20:30: `finalize_grace_seconds` can't be `None`: expected an int
                20 |     finalize_grace_seconds = None,
                   |                              ^^^^
                the IR's /roles/0/runtime/agent/finalize_grace_seconds
            @pumpkin//harness/claude_code/claude_code.star:80:26: `<` can't take `None` and an int: give it two numbers or two strings
                80 |     if max_output_tokens < 1:
                   |                          ^
                the left operand, `None`, is made at task.star:21:25
                the right operand, `1`, is made at @pumpkin//harness/claude_code/claude_code.star:80:28
                in `claude_code()`, called at task.star:13:9
            @pumpkin//harness/claude_code/claude_code.star:97:21: `+` can't take a string and an int: give it two numbers, two strings, two lists or two tuples
                97 |     ] + [name + "=" + model for name in _MODEL_VARIABLES] + [
                   |                     ^
                the left operand, `"ANTHROPIC_MODEL="`, is made at @pumpkin//harness/claude_code/claude_code.star:97:10
                the right operand, `5`, is made at task.star:15:13
                in `claude_code()`, called at task.star:13:9
            task.star:18:14: `stdin` can't be a list: expected a string
                18 |     prompt = [PROMPT],
                   |              ^^^^^^^^
                the IR's /roles/0/runtime/agent/stdin
            task.star:19:24: `deadline_seconds` can't be a string: expected an int
                19 |     deadline_seconds = "60",
                   |                        ^^^^
                the IR's /roles/0/runtime/agent/deadline_seconds
            @pumpkin//harness/claude_code/claude_code.star:80:26: `<` can't take a string and an int: give it two numbers or two strings
                80 |     if max_output_tokens < 1:
                   |                          ^
                the left operand, `"128000"`, is made at task.star:21:25
                the right operand, `1`, is made at @pumpkin//harness/claude_code/claude_code.star:80:28
                in `claude_code()`, called at task.star:13:9"#]],
    );
}
