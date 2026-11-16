//! `@pumpkin//harness/zcode/zcode.star`'s `zcode()`: the agent role it gives for each set of
//! arguments, and the arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;

use super::{FCS_PROMPT, agent, args, check, entry, evaluated, layer, route, show};

/// The arguments `zcode()` takes besides `SHARED`, each with the Starlark of its value.
const OWN: [(&str, &str); 2] = [
    ("context_tokens", r#"arg("context_tokens", int)"#),
    ("max_output_tokens", r#"arg("max_output_tokens", int)"#),
];

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    let cases = [
        [
            "glm-5.3",
            "max",
            "/workspace",
            FCS_PROMPT,
            "18000",
            "60",
            "1048576",
            "262144",
        ],
        [
            r#"vendor/"quoted"\model"#,
            "high",
            r#"/srv/my "work" dir"#,
            "-leading dash\nsecond line, \"quoted\"\n",
            "60",
            "0",
            "1",
            "1",
        ],
        [
            "model-small",
            "low",
            "/",
            "Go.",
            "60",
            "0",
            "200000",
            "32000",
        ],
    ];
    let mut shown = String::new();
    for (index, case) in cases.iter().enumerate() {
        let (workflow, images) = evaluated(&entry("zcode", &OWN), &args(&OWN, case)).unwrap();
        let [model, effort, workspace, _, _, _, context, output] = case;
        let (agent, runtime) = agent(&workflow, case);
        assert_eq!(runtime.stdin, None);
        let _ = writeln!(shown, "{model} {effort} {workspace} {context} {output}:");
        // Each file's whole text, but in the last case, whose files differ from the first's
        // only in the values the case names.
        shown.push_str(&show(runtime, |file| {
            if index == 2 {
                String::new()
            } else {
                format!("{}\n", file.content)
            }
        }));
        shown.push_str(&route(agent));
        if index == 0 {
            shown.push_str(&layer(agent, &images));
        }
    }
    check(
        &shown,
        &expect![[r#"
            glm-5.3 max /workspace 1048576 262144:
            argv: /harness/node-24.14.0/node /harness/zcode-0.16.9/zcode.cjs --mode yolo --cwd /workspace --disallowed-tools WebSearch,WebFetch "--prompt=Solve the evaluation task in /frontier/task/README.md.\n\nWhen the answer is ready, run `/.pumpkin/bin/sandboxctl call final <tar> --arg finalize=true`.\n"
            env: HOME=/workspace/.zcode-home TMPDIR=/workspace/.zcode-home/tmp ZCODE_DATA_BASE_DIR=/workspace/.zcode-home ZCODE_PERSONAL_PROVIDER_CONFIG_FILE=/workspace/.zcode-home/provider_config.json ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=/harness/zcode-0.16.9/provider/zcode-builtin.json
            cwd: /workspace
            dir 0700 /workspace/.zcode-home
            dir 0700 /workspace/.zcode-home/tmp
            dir 0700 /workspace/.zcode-home/.zcode
            dir 0700 /workspace/.zcode-home/.zcode/cli
            file 0600 /workspace/.zcode-home/.zcode/cli/config.json, sha256 45cd61429e1b7ef86da93ebc1e7d6d40c73cb4d49ed8876651fb6e58edbda724:
            {"features":{"mcp":false,"memory":false},"memory":{"use":false},"modelStream":{"idleTimeoutMs":3600000},"network":{"timeout":3600000},"permission":{"mode":"yolo"},"plugins":{"enabled":false},"skills":{"roots":[]}}
            file 0600 /workspace/.zcode-home/provider_config.json, sha256 e70714df76ed4e1cf03c707e1767d60b9a20db4933b3e62fb9488d2e9b3c34d2:
            {"config":{"defaultModelSelection":{"modelId":"glm-5.3","options":{"reasoningLevel":"max"},"providerId":"pumpkin"},"modelConfigRules":{"manualProviderModelRules":[],"providerModelRules":[{"config":{"enabled":true,"optionSpecs":{"maxOutputTokens":{"map":"{\"max_tokens\": maxOutputTokens}","max":262144},"reasoningLevel":{"map":"{\"reasoning_effort\": reasoningLevel}","values":["low","high","max"]}},"properties":{"contextWindow":1048576,"supportsJsonSchemaOutput":true,"supportsToolCall":true}},"modelId":"glm-5.3","providerId":"pumpkin"}]},"providerConfigRules":{"providerRules":[{"config":{"access":{"apiKey":"pumpkin","type":"api-key"},"api":{"baseUrl":"http://127.0.0.1:11235/v1","type":"openai-chat-completions"},"group":"standard-personal","modelOrder":["glm-5.3"],"personalModelIds":["glm-5.3"]},"providerId":"pumpkin","providerName":"Pumpkin"}]}},"schemaVersion":1}
            route: chat glm-5.3
            request headers: user-agent
            response headers: none
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/node/layer's Dockerfile, in pumpkin-library/harness/node/layer
            image 3: @pumpkin//harness/zcode/layer's Dockerfile, image 1 as base, image 2 as node, in pumpkin-library/harness/zcode/layer
            the agent's image: image 3
            vendor/"quoted"\model high /srv/my "work" dir 1 1:
            argv: /harness/node-24.14.0/node /harness/zcode-0.16.9/zcode.cjs --mode yolo --cwd "/srv/my \"work\" dir" --disallowed-tools WebSearch,WebFetch "--prompt=-leading dash\nsecond line, \"quoted\"\n"
            env: "HOME=/srv/my \"work\" dir/.zcode-home" "TMPDIR=/srv/my \"work\" dir/.zcode-home/tmp" "ZCODE_DATA_BASE_DIR=/srv/my \"work\" dir/.zcode-home" "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE=/srv/my \"work\" dir/.zcode-home/provider_config.json" ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=/harness/zcode-0.16.9/provider/zcode-builtin.json
            cwd: /srv/my "work" dir
            dir 0700 /srv/my "work" dir/.zcode-home
            dir 0700 /srv/my "work" dir/.zcode-home/tmp
            dir 0700 /srv/my "work" dir/.zcode-home/.zcode
            dir 0700 /srv/my "work" dir/.zcode-home/.zcode/cli
            file 0600 /srv/my "work" dir/.zcode-home/.zcode/cli/config.json, sha256 45cd61429e1b7ef86da93ebc1e7d6d40c73cb4d49ed8876651fb6e58edbda724:
            {"features":{"mcp":false,"memory":false},"memory":{"use":false},"modelStream":{"idleTimeoutMs":3600000},"network":{"timeout":3600000},"permission":{"mode":"yolo"},"plugins":{"enabled":false},"skills":{"roots":[]}}
            file 0600 /srv/my "work" dir/.zcode-home/provider_config.json, sha256 d458fa1ee220b1e1e9186c7debc76ef4ec75c9cf582bfd05e9554999400853b5:
            {"config":{"defaultModelSelection":{"modelId":"vendor/\"quoted\"\\model","options":{"reasoningLevel":"high"},"providerId":"pumpkin"},"modelConfigRules":{"manualProviderModelRules":[],"providerModelRules":[{"config":{"enabled":true,"optionSpecs":{"maxOutputTokens":{"map":"{\"max_tokens\": maxOutputTokens}","max":1},"reasoningLevel":{"map":"{\"reasoning_effort\": reasoningLevel}","values":["low","high","max"]}},"properties":{"contextWindow":1,"supportsJsonSchemaOutput":true,"supportsToolCall":true}},"modelId":"vendor/\"quoted\"\\model","providerId":"pumpkin"}]},"providerConfigRules":{"providerRules":[{"config":{"access":{"apiKey":"pumpkin","type":"api-key"},"api":{"baseUrl":"http://127.0.0.1:11235/v1","type":"openai-chat-completions"},"group":"standard-personal","modelOrder":["vendor/\"quoted\"\\model"],"personalModelIds":["vendor/\"quoted\"\\model"]},"providerId":"pumpkin","providerName":"Pumpkin"}]}},"schemaVersion":1}
            route: chat vendor/"quoted"\model
            model-small low / 200000 32000:
            argv: /harness/node-24.14.0/node /harness/zcode-0.16.9/zcode.cjs --mode yolo --cwd / --disallowed-tools WebSearch,WebFetch --prompt=Go.
            env: HOME=/.zcode-home TMPDIR=/.zcode-home/tmp ZCODE_DATA_BASE_DIR=/.zcode-home ZCODE_PERSONAL_PROVIDER_CONFIG_FILE=/.zcode-home/provider_config.json ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=/harness/zcode-0.16.9/provider/zcode-builtin.json
            cwd: /
            dir 0700 /.zcode-home
            dir 0700 /.zcode-home/tmp
            dir 0700 /.zcode-home/.zcode
            dir 0700 /.zcode-home/.zcode/cli
            file 0600 /.zcode-home/.zcode/cli/config.json, sha256 45cd61429e1b7ef86da93ebc1e7d6d40c73cb4d49ed8876651fb6e58edbda724:
            file 0600 /.zcode-home/provider_config.json, sha256 a2f63b274c27ed65648de3ffdec62b259a9ddbcb09b74c3516b97ffef9e52d51:
            route: chat model-small
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
        "200000",
        "32000",
    ];
    let mut refused = Vec::new();
    for (effort, workspace, prompt, context, output) in [
        ("medium", "/workspace", "Go.", "200000", "32000"),
        ("High", "/workspace", "Go.", "200000", "32000"),
        ("high", "/pumpkin/home", "Go.", "200000", "32000"),
        ("high", "/workspace", "", "200000", "32000"),
        ("high", "/workspace", "Go.", "0", "32000"),
        ("high", "/workspace", "Go.", "200000", "0"),
    ] {
        let case = [
            given[0], effort, workspace, prompt, given[4], given[5], context, output,
        ];
        refused.push(evaluated(&entry("zcode", &OWN), &args(&OWN, &case)).unwrap_err());
    }
    // Each argument that ZCode checks given `None`, as a task's `arg(…, default = None)` gives
    // it when no one passes one.
    for (argument, value) in [
        ("prompt = PROMPT,", "prompt = None,"),
        ("context_tokens = CONTEXT_TOKENS,", "context_tokens = None,"),
        (
            "max_output_tokens = MAX_OUTPUT_TOKENS,",
            "max_output_tokens = None,",
        ),
    ] {
        let entry = entry("zcode", &OWN).replacen(argument, value, 1);
        refused.push(evaluated(&entry, &args(&OWN, &given)).unwrap_err());
    }
    check(
        &refused.join("\n"),
        &expect![[r#"
            @pumpkin//harness/zcode/zcode.star:59:9: fail: `medium` is not a reasoning level of ZCode's: use one of low, high, max
                59 |         fail("`{}` is not a reasoning level of ZCode's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:59:9: fail: `High` is not a reasoning level of ZCode's: use one of low, high, max
                59 |         fail("`{}` is not a reasoning level of ZCode's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:69:13: `/pumpkin/home/.zcode-home` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                69 |     home = ("" if workspace == "/" else workspace) + "/.zcode-home"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/zcode/zcode.star:61:9: fail: ZCode runs only on a prompt: give a string that is not empty
                61 |         fail("ZCode runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:63:9: fail: `0` is not a number of tokens: give a positive int
                63 |         fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:65:9: fail: `0` is not a number of tokens: give a positive int
                65 |         fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:61:9: fail: ZCode runs only on a prompt: give a string that is not empty
                61 |         fail("ZCode runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:62:23: `<` can't take `None` and an int: give it two numbers or two strings
                62 |     if context_tokens < 1:
                   |                       ^
                the left operand, `None`, is made at task.star:24:22
                the right operand, `1`, is made at @pumpkin//harness/zcode/zcode.star:62:25
                in `zcode()`, called at task.star:16:9
            @pumpkin//harness/zcode/zcode.star:64:26: `<` can't take `None` and an int: give it two numbers or two strings
                64 |     if max_output_tokens < 1:
                   |                          ^
                the left operand, `None`, is made at task.star:25:25
                the right operand, `1`, is made at @pumpkin//harness/zcode/zcode.star:64:28
                in `zcode()`, called at task.star:16:9"#]],
    );
}
