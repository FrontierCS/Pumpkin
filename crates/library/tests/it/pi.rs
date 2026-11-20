//! `@pumpkin//harness/pi/pi.star`'s `pi()`: the agent role it gives for each set of arguments,
//! and the arguments it refuses.

use expect_test::expect;

use super::{FCS_PROMPT, check_refusals, check_roles};

/// The arguments `pi()` takes besides `SHARED`, each with the Starlark of its value.
const OWN: [(&str, &str); 2] = [
    ("context_tokens", r#"arg("context_tokens", int)"#),
    ("max_output_tokens", r#"arg("max_output_tokens", int)"#),
];

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    let cases = [
        [
            "gemini-4-argon",
            "high",
            "/workspace",
            FCS_PROMPT,
            "18000",
            "60",
            "1048576",
            "65536",
        ],
        [
            r#"vendor/"quoted"\model"#,
            "medium",
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
    // Each file's whole text, but in the last case, whose files differ from the first's
    // only in the values the case names.
    check_roles(
        "pi",
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
            gemini-4-argon high /workspace 1048576 65536:
            argv: /harness/node-24.14.0/node /harness/pi-1.2.0/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --print --offline --no-approve --provider pumpkin --model gemini-4-argon --thinking high --tools read,bash,edit,write --no-extensions --no-mcp --no-prompt-templates --no-themes --no-skills -- "Solve the evaluation task in /frontier/task/README.md.\n\nWhen the answer is ready, run `/.pumpkin/bin/sandboxctl call final <tar> --arg finalize=true`.\n"
            env: HOME=/workspace/.pi-home TMPDIR=/workspace/.pi-home/tmp PI_TELEMETRY=0
            cwd: /workspace
            dir 0700 /workspace/.pi-home
            dir 0700 /workspace/.pi-home/tmp
            dir 0700 /workspace/.pi-home/.pi
            dir 0700 /workspace/.pi-home/.pi/agent
            file 0600 /workspace/.pi-home/.pi/agent/models.json, sha256 8f22c8fa06691673e1530749fb1051785254df0ed2fd7083eaaa9852c6fb7074:
            {"providers":{"pumpkin":{"api":"openai-completions","apiKey":"pumpkin","baseUrl":"http://127.0.0.1:11235/v1","models":[{"compat":{"maxTokensField":"max_completion_tokens","supportsDeveloperRole":false,"supportsReasoningEffort":true,"supportsStore":false,"thinkingFormat":"openai"},"contextWindow":1048576,"id":"gemini-4-argon","maxTokens":65536,"reasoning":true,"thinkingLevelMap":{"high":"high","low":"low","medium":"medium"}}]}}}
            file 0600 /workspace/.pi-home/.pi/agent/settings.json, sha256 52f79489e16f036852e8e22f8c37060abe71316132b549bd41511fb1fef7e027:
            {"httpIdleTimeoutMs":3600000,"retry":{"maxRetries":16}}
            route: chat gemini-4-argon
            request headers: user-agent
            response headers: none
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/node/layer's Dockerfile, in pumpkin-library/harness/node/layer
            image 3: @pumpkin//harness/pi/layer's Dockerfile, image 1 as base, image 2 as node, in pumpkin-library/harness/pi/layer
            the agent's image: image 3
            vendor/"quoted"\model medium /srv/my "work" dir 1 1:
            argv: /harness/node-24.14.0/node /harness/pi-1.2.0/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --print --offline --no-approve --provider pumpkin --model vendor/"quoted"\model --thinking medium --tools read,bash,edit,write --no-extensions --no-mcp --no-prompt-templates --no-themes --no-skills -- "-leading dash\nsecond line, \"quoted\"\n"
            env: "HOME=/srv/my \"work\" dir/.pi-home" "TMPDIR=/srv/my \"work\" dir/.pi-home/tmp" PI_TELEMETRY=0
            cwd: /srv/my "work" dir
            dir 0700 /srv/my "work" dir/.pi-home
            dir 0700 /srv/my "work" dir/.pi-home/tmp
            dir 0700 /srv/my "work" dir/.pi-home/.pi
            dir 0700 /srv/my "work" dir/.pi-home/.pi/agent
            file 0600 /srv/my "work" dir/.pi-home/.pi/agent/models.json, sha256 c829254ad18abb2b5d9b92894ee29c63480454c2c1a4453135ef65ea8025bd8e:
            {"providers":{"pumpkin":{"api":"openai-completions","apiKey":"pumpkin","baseUrl":"http://127.0.0.1:11235/v1","models":[{"compat":{"maxTokensField":"max_completion_tokens","supportsDeveloperRole":false,"supportsReasoningEffort":true,"supportsStore":false,"thinkingFormat":"openai"},"contextWindow":1,"id":"vendor/\"quoted\"\\model","maxTokens":1,"reasoning":true,"thinkingLevelMap":{"high":"high","low":"low","medium":"medium"}}]}}}
            file 0600 /srv/my "work" dir/.pi-home/.pi/agent/settings.json, sha256 52f79489e16f036852e8e22f8c37060abe71316132b549bd41511fb1fef7e027:
            {"httpIdleTimeoutMs":3600000,"retry":{"maxRetries":16}}
            route: chat vendor/"quoted"\model
            model-small low / 200000 32000:
            argv: /harness/node-24.14.0/node /harness/pi-1.2.0/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --print --offline --no-approve --provider pumpkin --model model-small --thinking low --tools read,bash,edit,write --no-extensions --no-mcp --no-prompt-templates --no-themes --no-skills -- Go.
            env: HOME=/.pi-home TMPDIR=/.pi-home/tmp PI_TELEMETRY=0
            cwd: /
            dir 0700 /.pi-home
            dir 0700 /.pi-home/tmp
            dir 0700 /.pi-home/.pi
            dir 0700 /.pi-home/.pi/agent
            file 0600 /.pi-home/.pi/agent/models.json, sha256 8bba009e83c072a833ed2cf7a54ee81fe973da097a23f1859dc92474ea2a72a6:
            file 0600 /.pi-home/.pi/agent/settings.json, sha256 52f79489e16f036852e8e22f8c37060abe71316132b549bd41511fb1fef7e027:
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
    ];
    check_refusals(
        "pi",
        &OWN,
        &given,
        &[
            ("effort", "max"),
            ("effort", "High"),
            ("workspace", "/pumpkin/home"),
            ("prompt", ""),
            ("prompt", "@README.md"),
            ("context_tokens", "0"),
            ("max_output_tokens", "0"),
        ],
        &["prompt"],
        &expect![[r#"
            @pumpkin//harness/pi/pi.star:58:9: fail: `max` is not a thinking level of Pi's: use one of low, medium, high
                58 |         fail("`{}` is not a thinking level of Pi's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9
            @pumpkin//harness/pi/pi.star:58:9: fail: `High` is not a thinking level of Pi's: use one of low, medium, high
                58 |         fail("`{}` is not a thinking level of Pi's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9
            @pumpkin//harness/pi/pi.star:73:13: `/pumpkin/home/.pi-home` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                73 |     home = ("" if workspace == "/" else workspace) + "/.pi-home"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/pi/pi.star:60:9: fail: Pi runs only on a prompt: give a string that is not empty
                60 |         fail("Pi runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9
            @pumpkin//harness/pi/pi.star:64:9: fail: Pi reads a prompt that begins with `@` as a file's path: begin it with another character
                64 |         fail("Pi reads a prompt that begins with `@` as a file's path: begin it with another character")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9
            @pumpkin//harness/pi/pi.star:66:9: fail: `0` is not a number of tokens: give a positive int
                66 |         fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9
            @pumpkin//harness/pi/pi.star:68:9: fail: `0` is not a number of tokens: give a positive int
                68 |         fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9
            @pumpkin//harness/pi/pi.star:60:9: fail: Pi runs only on a prompt: give a string that is not empty
                60 |         fail("Pi runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `pi()`, called at task.star:16:9"#]],
    );
}
