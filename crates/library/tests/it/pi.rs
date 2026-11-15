//! `@pumpkin//harness/pi/pi.star`'s `pi()`: the agent role it gives for each set of arguments,
//! and the arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use schema::ir;

use super::zcode::{ENTRY, FCS_PROMPT, args};
use super::{check, evaluated, layer, route, show};

/// The task's entry, `ENTRY` of `pi()`.
fn entry() -> String {
    ENTRY.replace("zcode", "pi")
}

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
    let mut shown = String::new();
    for (index, case) in cases.iter().enumerate() {
        let (workflow, images) = evaluated(&entry(), &args(case)).unwrap();
        let [
            model,
            effort,
            workspace,
            _,
            deadline,
            grace,
            context,
            output,
        ] = case;
        let agent = &workflow.ir().roles[0];
        let ir::Runtime::Agent(runtime) = &agent.runtime else {
            panic!("the first role is the agent");
        };
        assert_eq!(runtime.stdin, None);
        assert_eq!(runtime.deadline_seconds.to_string(), *deadline);
        assert_eq!(runtime.finalize_grace_seconds.to_string(), *grace);
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
            shown.push_str(&layer(agent, images));
        }
    }
    check(
        &shown,
        &expect![[r#"
        gemini-4-argon high /workspace 1048576 65536:
        argv: /harness/node-24.14.0/node /harness/pi-1.2.0/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --mode json --offline --no-approve --provider pumpkin --model gemini-4-argon --thinking high --tools read,bash,edit,write --no-extensions --no-mcp --no-prompt-templates --no-themes --no-skills -- "Solve the evaluation task in /frontier/task/README.md.\n\nWhen the answer is ready, run `/.pumpkin/bin/sandboxctl submit final <tar> --finalize`.\n"
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
        image: @pumpkin//harness/node/layer's Dockerfile, in pumpkin-library/harness/node/layer
          FROM node@sha256:d8e448a56fc63242f70026718378bd4b00f8c82e78d20eefb199224a4d8e33d8
          ADD --checksum=sha256:38861b171079e440b25ac86483284f690d0ee7921dba98b0c8fe04a9b0a11667 https://github.com/NixOS/patchelf/releases/download/0.19.3/patchelf-0.19.3-x86_64.tar.gz /patchelf.tgz
          RUN mkdir /patchelf && tar --extract --gzip --file /patchelf.tgz --directory /patchelf ./bin/patchelf && mkdir --parents /out/harness/node-24.14.0/lib && cp /usr/local/bin/node /usr/local/LICENSE /out/harness/node-24.14.0/ && /patchelf/bin/patchelf --set-interpreter /harness/node-24.14.0/lib/ld-linux-x86-64.so.2 --set-rpath /harness/node-24.14.0/lib /out/harness/node-24.14.0/node && rm --recursive /patchelf /patchelf.tgz && cd /lib/x86_64-linux-gnu && cp --dereference ld-linux-x86-64.so.2 libc.so.6 libdl.so.2 libgcc_s.so.1 libm.so.6 libpthread.so.0 libstdc++.so.6 /out/harness/node-24.14.0/lib/ && cd /out/harness/node-24.14.0 && chmod 0444 LICENSE lib/libc.so.6 lib/libdl.so.2 lib/libgcc_s.so.1 lib/libm.so.6 lib/libpthread.so.0 lib/libstdc++.so.6 && chmod 0555 /out/harness . lib node lib/ld-linux-x86-64.so.2
        image: @pumpkin//harness/pi/layer's Dockerfile, image 1 as base, image 2 as node, in pumpkin-library/harness/pi/layer
          FROM node AS pi-1.2.0
          COPY package.json package-lock.json /layer/harness/pi-1.2.0/
          RUN cd /layer/harness/pi-1.2.0 && npm ci --ignore-scripts --no-audit --no-fund && chmod --recursive a=rX /layer/harness
          FROM base
          COPY --link --from=node --chown=0:0 /out/ /
          COPY --link --from=pi-1.2.0 --chown=0:0 /layer/ /
        vendor/"quoted"\model medium /srv/my "work" dir 1 1:
        argv: /harness/node-24.14.0/node /harness/pi-1.2.0/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --mode json --offline --no-approve --provider pumpkin --model vendor/"quoted"\model --thinking medium --tools read,bash,edit,write --no-extensions --no-mcp --no-prompt-templates --no-themes --no-skills -- "-leading dash\nsecond line, \"quoted\"\n"
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
        argv: /harness/node-24.14.0/node /harness/pi-1.2.0/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js --mode json --offline --no-approve --provider pumpkin --model model-small --thinking low --tools read,bash,edit,write --no-extensions --no-mcp --no-prompt-templates --no-themes --no-skills -- Go.
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
        ("max", "/workspace", "Go.", "200000", "32000"),
        ("High", "/workspace", "Go.", "200000", "32000"),
        ("high", "/pumpkin/home", "Go.", "200000", "32000"),
        ("high", "/workspace", "", "200000", "32000"),
        ("high", "/workspace", "@README.md", "200000", "32000"),
        ("high", "/workspace", "Go.", "0", "32000"),
        ("high", "/workspace", "Go.", "200000", "0"),
    ] {
        let case = [
            given[0], effort, workspace, prompt, given[4], given[5], context, output,
        ];
        refused.push(evaluated(&entry(), &args(&case)).unwrap_err());
    }
    // Each argument of Pi's own given `None`, as a task's `arg(…, default = None)` gives it when
    // no one passes one.
    for (argument, value) in [
        ("prompt = PROMPT,", "prompt = None,"),
        ("context_tokens = CONTEXT_TOKENS,", "context_tokens = None,"),
        ("output_tokens = OUTPUT_TOKENS,", "output_tokens = None,"),
    ] {
        let entry = entry().replacen(argument, value, 1);
        refused.push(evaluated(&entry, &args(&given)).unwrap_err());
    }
    check(
        &refused.join("\n"),
        &expect![[r#"
        @pumpkin//harness/pi/pi.star:61:9: fail: `max` is not a thinking level of Pi's: use one of low, medium, high
            61 |         fail("`{}` is not a thinking level of Pi's: use one of {}".format(effort, ", ".join(_EFFORTS)))
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:61:9: fail: `High` is not a thinking level of Pi's: use one of low, medium, high
            61 |         fail("`{}` is not a thinking level of Pi's: use one of {}".format(effort, ", ".join(_EFFORTS)))
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:76:13: `/pumpkin/home/.pi-home` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
            76 |     home = ("" if workspace == "/" else workspace) + "/.pi-home"
               |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            the IR's /roles/0/runtime/agent/dirs/0/path
        @pumpkin//harness/pi/pi.star:63:9: fail: Pi runs only on a prompt: give a string that is not empty
            63 |         fail("Pi runs only on a prompt: give a string that is not empty")
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:67:9: fail: Pi reads a prompt that begins with `@` as a file's path: begin it with another character
            67 |         fail("Pi reads a prompt that begins with `@` as a file's path: begin it with another character")
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:69:9: fail: `0` is not a number of tokens: give a positive int
            69 |         fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:71:9: fail: `0` is not a number of tokens: give a positive int
            71 |         fail("`{}` is not a number of tokens: give a positive int".format(output_tokens))
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:63:9: fail: Pi runs only on a prompt: give a string that is not empty
            63 |         fail("Pi runs only on a prompt: give a string that is not empty")
               |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:68:23: `<` can't take `None` and an int: give it two ints or two strings
            68 |     if context_tokens < 1:
               |                       ^
            the left operand, `None`, is made at task.star:23:22
            the right operand, `1`, is made at @pumpkin//harness/pi/pi.star:68:25
            in `pi()`, called at task.star:15:9
        @pumpkin//harness/pi/pi.star:70:22: `<` can't take `None` and an int: give it two ints or two strings
            70 |     if output_tokens < 1:
               |                      ^
            the left operand, `None`, is made at task.star:24:21
            the right operand, `1`, is made at @pumpkin//harness/pi/pi.star:70:24
            in `pi()`, called at task.star:15:9"#]],
    );
}
