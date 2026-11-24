//! `@pumpkin//harness/qwen_code/qwen_code.star`'s `qwen_code()`: the agent role it gives for each
//! set of arguments, and the arguments it refuses.

use expect_test::expect;

use super::{TASK_PROMPT, check_refusals, check_roles};

/// The arguments `qwen_code()` takes besides `SHARED`, each with the Starlark of its value.
const OWN: [(&str, &str); 3] = [
    ("context_tokens", r#"arg("context_tokens", int)"#),
    ("max_output_tokens", r#"arg("max_output_tokens", int)"#),
    ("extra_body", r#"json.decode(arg("extra_body", str))"#),
];

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    let cases = [
        [
            "qwen3.8-max",
            "high",
            "/workspace",
            TASK_PROMPT,
            "18000",
            "60",
            "1000000",
            "65536",
            r#"{"enable_thinking": true, "temperature": 1}"#,
        ],
        // The library's own two fields win over the same fields of `extra_body`.
        [
            r#"vendor/"quoted"\model"#,
            "xhigh",
            r#"/srv/my "work" dir"#,
            "-leading dash\nsecond line, \"quoted\"\n",
            "60",
            "0",
            "1",
            "1",
            r#"{"max_tokens": 7, "reasoning_effort": "none"}"#,
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
            "{}",
        ],
    ];
    // The settings' whole text, but in the last case, whose settings differ from the
    // first's only in the values the case names.
    check_roles(
        "qwen_code",
        &OWN,
        &cases,
        false,
        |index, file| {
            if index == 2 {
                String::new()
            } else {
                format!("{}\n", file.content)
            }
        },
        &expect![[r#"
            qwen3.8-max high /workspace 1000000 65536 {"enable_thinking": true, "temperature": 1}:
            argv: /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --auth-type openai --model qwen3.8-max --approval-mode yolo --output-format text "--prompt=Solve the evaluation task in /task/README.md.\n\nWhen the answer is ready, run `/.pumpkin/bin/sandboxctl call final <tar> --arg finalize=true`.\n"
            env: HOME=/workspace/.qwen-home TMPDIR=/workspace/.qwen-home/tmp QWEN_HOME=/workspace/.qwen-home/qwen PUMPKIN_MODEL_KEY=pumpkin QWEN_STREAM_MAX_LIFETIME_MS=0 QWEN_CODE_MODELS_DEV_REFRESH=off
            cwd: /workspace
            dir 0700 /workspace/.qwen-home
            dir 0700 /workspace/.qwen-home/tmp
            dir 0700 /workspace/.qwen-home/qwen
            file 0600 /workspace/.qwen-home/qwen/settings.json:
            {"$version":4,"general":{"enableAutoUpdate":false},"mcpServers":{},"memory":{"enableAutoSkill":false,"enableManagedAutoDream":false,"enableManagedAutoMemory":false},"model":{"name":"qwen3.8-max"},"modelProviders":{"openai":[{"baseUrl":"http://127.0.0.1:11235/v1","envKey":"PUMPKIN_MODEL_KEY","generationConfig":{"contextWindowSize":1000000,"extra_body":{"enable_thinking":true,"max_tokens":65536,"reasoning_effort":"high","temperature":1},"maxRetries":16,"streamIdleTimeoutMs":3600000,"timeout":3600000},"id":"qwen3.8-max","wireApi":"chat-completions"}]},"privacy":{"usageStatisticsEnabled":false},"security":{"auth":{"selectedType":"openai"}},"skills":{"disabledLevels":["project","extension","bundled"]},"telemetry":{"enabled":false},"tools":{"disabled":["web_fetch","web_search"],"shell":{"defaultTimeoutMs":0}}}
            route: chat qwen3.8-max
            request headers: user-agent
            response headers: none
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/node/layer's Dockerfile, in pumpkin-library/harness/node/layer
            image 3: @pumpkin//harness/qwen_code/layer's Dockerfile, image 1 as base, image 2 as node, in pumpkin-library/harness/qwen_code/layer
            the agent's image: image 3
            vendor/"quoted"\model xhigh /srv/my "work" dir 1 1 {"max_tokens": 7, "reasoning_effort": "none"}:
            argv: /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --auth-type openai --model vendor/"quoted"\model --approval-mode yolo --output-format text "--prompt=-leading dash\nsecond line, \"quoted\"\n"
            env: "HOME=/srv/my \"work\" dir/.qwen-home" "TMPDIR=/srv/my \"work\" dir/.qwen-home/tmp" "QWEN_HOME=/srv/my \"work\" dir/.qwen-home/qwen" PUMPKIN_MODEL_KEY=pumpkin QWEN_STREAM_MAX_LIFETIME_MS=0 QWEN_CODE_MODELS_DEV_REFRESH=off
            cwd: /srv/my "work" dir
            dir 0700 /srv/my "work" dir/.qwen-home
            dir 0700 /srv/my "work" dir/.qwen-home/tmp
            dir 0700 /srv/my "work" dir/.qwen-home/qwen
            file 0600 /srv/my "work" dir/.qwen-home/qwen/settings.json:
            {"$version":4,"general":{"enableAutoUpdate":false},"mcpServers":{},"memory":{"enableAutoSkill":false,"enableManagedAutoDream":false,"enableManagedAutoMemory":false},"model":{"name":"vendor/\"quoted\"\\model"},"modelProviders":{"openai":[{"baseUrl":"http://127.0.0.1:11235/v1","envKey":"PUMPKIN_MODEL_KEY","generationConfig":{"contextWindowSize":1,"extra_body":{"max_tokens":1,"reasoning_effort":"xhigh"},"maxRetries":16,"streamIdleTimeoutMs":3600000,"timeout":3600000},"id":"vendor/\"quoted\"\\model","wireApi":"chat-completions"}]},"privacy":{"usageStatisticsEnabled":false},"security":{"auth":{"selectedType":"openai"}},"skills":{"disabledLevels":["project","extension","bundled"]},"telemetry":{"enabled":false},"tools":{"disabled":["web_fetch","web_search"],"shell":{"defaultTimeoutMs":0}}}
            route: chat vendor/"quoted"\model
            model-small low / 200000 32000 {}:
            argv: /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --auth-type openai --model model-small --approval-mode yolo --output-format text --prompt=Go.
            env: HOME=/.qwen-home TMPDIR=/.qwen-home/tmp QWEN_HOME=/.qwen-home/qwen PUMPKIN_MODEL_KEY=pumpkin QWEN_STREAM_MAX_LIFETIME_MS=0 QWEN_CODE_MODELS_DEV_REFRESH=off
            cwd: /
            dir 0700 /.qwen-home
            dir 0700 /.qwen-home/tmp
            dir 0700 /.qwen-home/qwen
            file 0600 /.qwen-home/qwen/settings.json:
            route: chat model-small
        "#]],
    );
}

#[test]
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
        "{}",
    ];
    check_refusals(
        "qwen_code",
        &OWN,
        &given,
        &[
            ("effort", "none"),
            ("effort", "High"),
            ("workspace", "/pumpkin/home"),
            ("prompt", ""),
            ("context_tokens", "0"),
            ("max_output_tokens", "0"),
        ],
        &["prompt"],
        &expect![[r#"
            @pumpkin//harness/qwen_code/qwen_code.star:60:9: fail: `none` is not a reasoning level of Qwen Code's: use one of low, medium, high, xhigh, max
                60 |         fail("`{}` is not a reasoning level of Qwen Code's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:17:9
            @pumpkin//harness/qwen_code/qwen_code.star:60:9: fail: `High` is not a reasoning level of Qwen Code's: use one of low, medium, high, xhigh, max
                60 |         fail("`{}` is not a reasoning level of Qwen Code's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:17:9
            @pumpkin//harness/qwen_code/qwen_code.star:70:13: `/pumpkin/home/.qwen-home` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                70 |     home = ("" if workspace == "/" else workspace) + "/.qwen-home"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/qwen_code/qwen_code.star:62:9: fail: Qwen Code runs only on a prompt: give a string that is not empty
                62 |         fail("Qwen Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:17:9
            @pumpkin//harness/qwen_code/qwen_code.star:64:9: fail: `0` is not a number of tokens: give a positive int
                64 |         fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:17:9
            @pumpkin//harness/qwen_code/qwen_code.star:66:9: fail: `0` is not a number of tokens: give a positive int
                66 |         fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:17:9
            @pumpkin//harness/qwen_code/qwen_code.star:62:9: fail: Qwen Code runs only on a prompt: give a string that is not empty
                62 |         fail("Qwen Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:17:9"#]],
    );
}
