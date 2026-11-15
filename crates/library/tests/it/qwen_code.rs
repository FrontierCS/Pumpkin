//! `@pumpkin//harness/qwen_code/qwen_code.star`'s `qwen_code()`: the agent role it gives for each
//! set of arguments, and the arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use lang::Args;
use schema::ir;

use super::{check, evaluated, layer, route, show};

/// A task's entry that gives `qwen_code()` each of its arguments from an `--arg` of its name, and
/// builds its agent on the image of `//images/Dockerfile`, which the judge runs on too.
const ENTRY: &str = r#"
load("@pumpkin//harness/qwen_code/qwen_code.star", "qwen_code")

MODEL = arg("model", str)
EFFORT = arg("effort", str)
WORKSPACE = arg("workspace", str)
PROMPT = arg("prompt", str)
DEADLINE = arg("deadline_seconds", int)
GRACE = arg("finalize_grace_seconds", int)
CONTEXT_TOKENS = arg("context_tokens", int)
MAX_OUTPUT_TOKENS = arg("max_output_tokens", int)
EXTRA_BODY = json.decode(arg("extra_body", str))

BASE = image("Dockerfile", context = "//images")

agent = qwen_code(
    BASE,
    model = MODEL,
    effort = EFFORT,
    workspace = WORKSPACE,
    prompt = PROMPT,
    deadline_seconds = DEADLINE,
    finalize_grace_seconds = GRACE,
    context_tokens = CONTEXT_TOKENS,
    max_output_tokens = MAX_OUTPUT_TOKENS,
    extra_body = EXTRA_BODY,
)

workflow(
    name = "qwen-code",
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
const NAMES: [&str; 9] = [
    "model",
    "effort",
    "workspace",
    "prompt",
    "deadline_seconds",
    "finalize_grace_seconds",
    "context_tokens",
    "max_output_tokens",
    "extra_body",
];

/// The arguments of `case`, by their names, as `ENTRY` takes them.
fn args(case: &[&str; 9]) -> Args {
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
    let cases = [
        [
            "qwen3.8-max",
            "high",
            "/workspace",
            FCS_PROMPT,
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
    let mut shown = String::new();
    for (index, case) in cases.iter().enumerate() {
        let (workflow, images) = evaluated(ENTRY, &args(case)).unwrap();
        let [
            model,
            effort,
            workspace,
            _,
            deadline,
            grace,
            context,
            output,
            body,
        ] = case;
        let agent = &workflow.ir().roles[0];
        let ir::Runtime::Agent(runtime) = &agent.runtime else {
            panic!("the first role is the agent");
        };
        assert_eq!(runtime.stdin, None);
        assert_eq!(runtime.deadline_seconds.to_string(), *deadline);
        assert_eq!(runtime.finalize_grace_seconds.to_string(), *grace);
        let _ = writeln!(
            shown,
            "{model} {effort} {workspace} {context} {output} {body}:"
        );
        // The settings' whole text, but in the last case, whose settings differ from the
        // first's only in the values the case names.
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
            qwen3.8-max high /workspace 1000000 65536 {"enable_thinking": true, "temperature": 1}:
            argv: /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --auth-type openai --model qwen3.8-max --approval-mode yolo --output-format stream-json "--prompt=Solve the evaluation task in /frontier/task/README.md.\n\nWhen the answer is ready, run `/.pumpkin/bin/sandboxctl submit final <tar> --finalize`.\n"
            env: HOME=/workspace/.qwen-home TMPDIR=/workspace/.qwen-home/tmp QWEN_HOME=/workspace/.qwen-home/qwen PUMPKIN_MODEL_KEY=pumpkin QWEN_STREAM_MAX_LIFETIME_MS=0 QWEN_CODE_MODELS_DEV_REFRESH=off
            cwd: /workspace
            dir 0700 /workspace/.qwen-home
            dir 0700 /workspace/.qwen-home/tmp
            dir 0700 /workspace/.qwen-home/qwen
            file 0600 /workspace/.qwen-home/qwen/settings.json, sha256 6657c75c768d967084049d7f3d118bf05fab4b7e9bec8a79a29003eea2ad8cd8:
            {"$version":4,"general":{"enableAutoUpdate":false},"mcpServers":{},"memory":{"enableAutoSkill":false,"enableManagedAutoDream":false,"enableManagedAutoMemory":false},"model":{"name":"qwen3.8-max"},"modelProviders":{"openai":[{"baseUrl":"http://127.0.0.1:11235/v1","envKey":"PUMPKIN_MODEL_KEY","generationConfig":{"contextWindowSize":1000000,"extra_body":{"enable_thinking":true,"max_tokens":65536,"reasoning_effort":"high","temperature":1},"maxRetries":16,"streamIdleTimeoutMs":3600000,"timeout":3600000},"id":"qwen3.8-max","wireApi":"chat-completions"}]},"privacy":{"usageStatisticsEnabled":false},"security":{"auth":{"selectedType":"openai"}},"skills":{"disabledLevels":["project","extension","bundled"]},"telemetry":{"enabled":false},"tools":{"disabled":["web_fetch","web_search"],"shell":{"defaultTimeoutMs":0}}}
            route: chat qwen3.8-max
            request headers: user-agent
            response headers: none
            image: @pumpkin//harness/node/layer's Dockerfile, in pumpkin-library/harness/node/layer
              FROM node@sha256:d8e448a56fc63242f70026718378bd4b00f8c82e78d20eefb199224a4d8e33d8
              ADD --checksum=sha256:38861b171079e440b25ac86483284f690d0ee7921dba98b0c8fe04a9b0a11667 https://github.com/NixOS/patchelf/releases/download/0.19.3/patchelf-0.19.3-x86_64.tar.gz /patchelf.tgz
              RUN mkdir /patchelf && tar --extract --gzip --file /patchelf.tgz --directory /patchelf ./bin/patchelf && mkdir --parents /out/harness/node-24.14.0/lib && cp /usr/local/bin/node /usr/local/LICENSE /out/harness/node-24.14.0/ && /patchelf/bin/patchelf --set-interpreter /harness/node-24.14.0/lib/ld-linux-x86-64.so.2 --set-rpath /harness/node-24.14.0/lib /out/harness/node-24.14.0/node && rm --recursive /patchelf /patchelf.tgz && cd /lib/x86_64-linux-gnu && cp --dereference ld-linux-x86-64.so.2 libc.so.6 libdl.so.2 libgcc_s.so.1 libm.so.6 libpthread.so.0 libstdc++.so.6 /out/harness/node-24.14.0/lib/ && cd /out/harness/node-24.14.0 && chmod 0444 LICENSE lib/libc.so.6 lib/libdl.so.2 lib/libgcc_s.so.1 lib/libm.so.6 lib/libpthread.so.0 lib/libstdc++.so.6 && chmod 0555 /out/harness . lib node lib/ld-linux-x86-64.so.2
            image: @pumpkin//harness/qwen_code/layer's Dockerfile, image 1 as base, image 2 as node, in pumpkin-library/harness/qwen_code/layer
              FROM node AS qwen-code-0.25.0
              ADD --checksum=sha256:afeab0c85d682f201101319ce05decf3302d7ed7fb6319baa6a592051ea1a1ba https://registry.npmjs.org/@qwen-code/qwen-code/-/qwen-code-0.25.0.tgz /qwen-code.tgz
              RUN mkdir --parents /layer/harness/qwen-code-0.25.0 && tar --extract --gzip --file /qwen-code.tgz --directory /layer/harness/qwen-code-0.25.0 --strip-components=1 --no-same-owner && chmod --recursive a=rX /layer/harness && chmod 0555 /layer/harness/qwen-code-0.25.0/vendor/ripgrep/x64-linux/rg
              FROM base
              COPY --link --from=node --chown=0:0 /out/ /
              COPY --link --from=qwen-code-0.25.0 --chown=0:0 /layer/ /
            vendor/"quoted"\model xhigh /srv/my "work" dir 1 1 {"max_tokens": 7, "reasoning_effort": "none"}:
            argv: /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --auth-type openai --model vendor/"quoted"\model --approval-mode yolo --output-format stream-json "--prompt=-leading dash\nsecond line, \"quoted\"\n"
            env: "HOME=/srv/my \"work\" dir/.qwen-home" "TMPDIR=/srv/my \"work\" dir/.qwen-home/tmp" "QWEN_HOME=/srv/my \"work\" dir/.qwen-home/qwen" PUMPKIN_MODEL_KEY=pumpkin QWEN_STREAM_MAX_LIFETIME_MS=0 QWEN_CODE_MODELS_DEV_REFRESH=off
            cwd: /srv/my "work" dir
            dir 0700 /srv/my "work" dir/.qwen-home
            dir 0700 /srv/my "work" dir/.qwen-home/tmp
            dir 0700 /srv/my "work" dir/.qwen-home/qwen
            file 0600 /srv/my "work" dir/.qwen-home/qwen/settings.json, sha256 21ef41778343c9df2d2b71d1d136e43c50b3d965692939fc9900f90636ee7520:
            {"$version":4,"general":{"enableAutoUpdate":false},"mcpServers":{},"memory":{"enableAutoSkill":false,"enableManagedAutoDream":false,"enableManagedAutoMemory":false},"model":{"name":"vendor/\"quoted\"\\model"},"modelProviders":{"openai":[{"baseUrl":"http://127.0.0.1:11235/v1","envKey":"PUMPKIN_MODEL_KEY","generationConfig":{"contextWindowSize":1,"extra_body":{"max_tokens":1,"reasoning_effort":"xhigh"},"maxRetries":16,"streamIdleTimeoutMs":3600000,"timeout":3600000},"id":"vendor/\"quoted\"\\model","wireApi":"chat-completions"}]},"privacy":{"usageStatisticsEnabled":false},"security":{"auth":{"selectedType":"openai"}},"skills":{"disabledLevels":["project","extension","bundled"]},"telemetry":{"enabled":false},"tools":{"disabled":["web_fetch","web_search"],"shell":{"defaultTimeoutMs":0}}}
            route: chat vendor/"quoted"\model
            model-small low / 200000 32000 {}:
            argv: /harness/node-24.14.0/node /harness/qwen-code-0.25.0/cli-entry.js --auth-type openai --model model-small --approval-mode yolo --output-format stream-json --prompt=Go.
            env: HOME=/.qwen-home TMPDIR=/.qwen-home/tmp QWEN_HOME=/.qwen-home/qwen PUMPKIN_MODEL_KEY=pumpkin QWEN_STREAM_MAX_LIFETIME_MS=0 QWEN_CODE_MODELS_DEV_REFRESH=off
            cwd: /
            dir 0700 /.qwen-home
            dir 0700 /.qwen-home/tmp
            dir 0700 /.qwen-home/qwen
            file 0600 /.qwen-home/qwen/settings.json, sha256 9537e01ef9bc6a1125256ac1dab1f4a8b590ef2ed292782601344c0c829246ab:
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
        "{}",
    ];
    let mut refused = Vec::new();
    for (effort, workspace, prompt, context, output) in [
        ("none", "/workspace", "Go.", "200000", "32000"),
        ("High", "/workspace", "Go.", "200000", "32000"),
        ("high", "/pumpkin/home", "Go.", "200000", "32000"),
        ("high", "/workspace", "", "200000", "32000"),
        ("high", "/workspace", "Go.", "0", "32000"),
        ("high", "/workspace", "Go.", "200000", "0"),
    ] {
        let case = [
            given[0], effort, workspace, prompt, given[4], given[5], context, output, given[8],
        ];
        refused.push(evaluated(ENTRY, &args(&case)).unwrap_err());
    }
    // Each argument that Qwen Code checks given `None`, as a task's `arg(…, default = None)`
    // gives it when no one passes one.
    for (argument, value) in [
        ("prompt = PROMPT,", "prompt = None,"),
        ("context_tokens = CONTEXT_TOKENS,", "context_tokens = None,"),
        (
            "max_output_tokens = MAX_OUTPUT_TOKENS,",
            "max_output_tokens = None,",
        ),
        ("extra_body = EXTRA_BODY,", "extra_body = None,"),
    ] {
        let entry = ENTRY.replacen(argument, value, 1);
        refused.push(evaluated(&entry, &args(&given)).unwrap_err());
    }
    check(
        &refused.join("\n"),
        &expect![[r#"
            @pumpkin//harness/qwen_code/qwen_code.star:61:9: fail: `none` is not a reasoning level of Qwen Code's: use one of low, medium, high, xhigh, max
                61 |         fail("`{}` is not a reasoning level of Qwen Code's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:61:9: fail: `High` is not a reasoning level of Qwen Code's: use one of low, medium, high, xhigh, max
                61 |         fail("`{}` is not a reasoning level of Qwen Code's: use one of {}".format(effort, ", ".join(_EFFORTS)))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:71:13: `/pumpkin/home/.qwen-home` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                71 |     home = ("" if workspace == "/" else workspace) + "/.qwen-home"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/qwen_code/qwen_code.star:63:9: fail: Qwen Code runs only on a prompt: give a string that is not empty
                63 |         fail("Qwen Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:65:9: fail: `0` is not a number of tokens: give a positive int
                65 |         fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:67:9: fail: `0` is not a number of tokens: give a positive int
                67 |         fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:63:9: fail: Qwen Code runs only on a prompt: give a string that is not empty
                63 |         fail("Qwen Code runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:64:23: `<` can't take `None` and an int: give it two ints or two strings
                64 |     if context_tokens < 1:
                   |                       ^
                the left operand, `None`, is made at task.star:24:22
                the right operand, `1`, is made at @pumpkin//harness/qwen_code/qwen_code.star:64:25
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:66:26: `<` can't take `None` and an int: give it two ints or two strings
                66 |     if max_output_tokens < 1:
                   |                          ^
                the left operand, `None`, is made at task.star:25:25
                the right operand, `1`, is made at @pumpkin//harness/qwen_code/qwen_code.star:66:28
                in `qwen_code()`, called at task.star:16:9
            @pumpkin//harness/qwen_code/qwen_code.star:74:23: `|` can't take `None` and a dict: give it two dicts
                74 |     body = extra_body | {"reasoning_effort": effort, "max_tokens": max_output_tokens}
                   |                       ^
                the left operand, `None`, is made at task.star:26:18
                the right operand, a dict, is made at @pumpkin//harness/qwen_code/qwen_code.star:74:25
                in `qwen_code()`, called at task.star:16:9"#]],
    );
}
