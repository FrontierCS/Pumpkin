//! `@pumpkin//harness/codex/codex.star`'s `codex()`: the agent role it gives for each set of
//! arguments that Pumpkin's examples and tests and the task repository give it, and the
//! arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use lang::Args;
use schema::ir;
use serde_json::Value;

use super::{check, evaluated, layer, route, show};

/// A task's entry that gives `codex()` each of its arguments from an `--arg` of its name, and
/// builds its agent on the image of `//images/Dockerfile`, which the judge runs on too.
const ENTRY: &str = r#"
load("@pumpkin//harness/codex/codex.star", "codex")

MODEL = arg("model", str)
EFFORT = arg("effort", str)
TOOLS = arg("tools", str)
WORKSPACE = arg("workspace", str)
PROMPT = arg("prompt", str)
DEADLINE = arg("deadline_seconds", int)
GRACE = arg("finalize_grace_seconds", int)

BASE = image("Dockerfile", context = "//images")

agent = codex(
    BASE,
    model = MODEL,
    effort = EFFORT,
    tools = TOOLS,
    workspace = WORKSPACE,
    prompt = PROMPT,
    deadline_seconds = DEADLINE,
    finalize_grace_seconds = GRACE,
)

workflow(
    name = "codex",
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

/// The prompt of the task repository's `lib/fcs.star`.
const FCS_PROMPT: &str = "Solve the evaluation task in /frontier/task/README.md.

Before doing any task work, read that file completely. It is authoritative for the exact train
and final tar layouts, and for how to submit with `/.pumpkin/bin/sandboxctl`. Work only in
/workspace. Submit a tar for train verification, inspect its score and remaining calls with
`sandboxctl`, and iterate while useful. Do not write a replacement submission client.

When the answer is ready, create the final tar in the task-defined format, run
`/.pumpkin/bin/sandboxctl submit final <tar> --finalize` exactly once as your last platform
action, then exit.
Web search is disabled; use only local task files and tools.
";

/// Model, effort, tools, workspace, prompt, deadline and grace, from `examples/codex`, `pumpkin`'s
/// tests and the task repository, on each model of the catalog; then other workspaces and
/// prompts, each effort, a model that needs escaping, and the root as the workspace.
#[expect(clippy::too_many_lines, reason = "it is the cases")]
fn cases() -> Vec<[String; 7]> {
    let greeting = "Write a greeting to the file `greeting`, then submit it with `sandboxctl \
                    submit final greeting --finalize`.\n";
    let fixture = "Solve the task in /task/README.md, then submit with \"sandboxctl\".\n";
    let mut cases = vec![
        [
            "model-small",
            "high",
            "native",
            "/workspace",
            greeting,
            "1800",
            "60",
        ],
        [
            "model-small",
            "high",
            "native",
            "/workspace",
            fixture,
            "18000",
            "0",
        ],
        ["model-small", "high", "native", "/workspace", "", "60", "0"],
        [
            "model-large",
            "low",
            "native",
            "/home/agent",
            "Solve it.",
            "60",
            "0",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "functions",
            "/workspace",
            FCS_PROMPT,
            "18000",
            "60",
        ],
        [
            "glm-5.3",
            "xhigh",
            "functions",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "native",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
        ],
        [
            "glm-5.3",
            "high",
            "native",
            "/workspace",
            "Go.\n",
            "3600",
            "60",
        ],
        [
            "model-small",
            "xhigh",
            "native",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
        ],
        [
            r#"vendor/"quoted"\model"#,
            "xhigh",
            "native",
            "/srv/work",
            "Go.\n",
            "60",
            "0",
        ],
        ["glm-5.3", "high", "functions", "/", "Go.\n", "60", "0"],
    ];
    for effort in [
        "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ] {
        cases.push([
            "gpt-6.1-terra",
            effort,
            "functions",
            "/workspace",
            "Go.\n",
            "60",
            "0",
        ]);
    }
    (cases.into_iter())
        .map(|case| case.map(str::to_owned))
        .collect()
}

/// The arguments of `case`, by their names, as `ENTRY` takes them.
fn args(case: &[String; 7]) -> Args {
    let names = [
        "model",
        "effort",
        "tools",
        "workspace",
        "prompt",
        "deadline_seconds",
        "finalize_grace_seconds",
    ];
    let given: Vec<String> = (names.iter().zip(case))
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    Args::parse(given.iter().map(String::as_str)).unwrap()
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    let mut shown = String::new();
    for (index, case) in cases().into_iter().enumerate() {
        let (workflow, images) = evaluated(ENTRY, &args(&case)).unwrap();
        let [model, effort, tools, workspace, prompt, deadline, grace] = &case;
        let agent = &workflow.ir().roles[0];
        let ir::Runtime::Agent(runtime) = &agent.runtime else {
            panic!("the first role is the agent");
        };
        assert_eq!(runtime.stdin.as_deref(), Some(prompt.as_str()));
        assert_eq!(runtime.deadline_seconds.to_string(), *deadline);
        assert_eq!(runtime.finalize_grace_seconds.to_string(), *grace);
        let _ = writeln!(shown, "{model} {effort} {tools} {workspace}:");
        shown.push_str(&show(runtime, excerpt));
        shown.push_str(&route(agent));
        // The layer and the configuration's whole text, once: the cases' configurations differ
        // only in the lines that name a model.
        if index == 0 {
            shown.push_str(&layer(agent, images));
            let config = &runtime.files[0];
            let _ = write!(shown, "{} in full:\n{}", config.path, config.content);
        }
    }
    check(
        &shown,
        &expect![[r#"
        model-small high native /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 ee811c2e99a8ede9e80570c6a373574bbcc8ee3e3fdae4a49588041615226810:
          model = "model-small"
          model_reasoning_effort = "high"
          model_provider = "pumpkin"
        route: responses model-small
        request headers: originator user-agent session-id thread-id x-codex-beta-features x-codex-window-id x-codex-turn-metadata x-openai-internal-codex-responses-lite
        response headers: openai-model x-reasoning-included x-models-etag x-codex-turn-state x-codex-safety-buffering-enabled x-codex-safety-buffering-faster-model x-codex-credits-has-credits x-codex-credits-unlimited x-codex-credits-balance x-codex-promo-message x-codex-rate-limit-reached-type x-codex-active-limit x-oai-request-id cf-ray x-openai-authorization-error x-error-json x-codex-primary-used-percent x-codex-primary-window-minutes x-codex-primary-reset-at x-codex-secondary-used-percent x-codex-secondary-window-minutes x-codex-secondary-reset-at x-codex-limit-name
        image: @pumpkin//harness/codex/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/codex/layer
          FROM busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS codex-0.167.0
          ADD --checksum=sha256:2dc9aa871abb513c3539d20d025f701faf1974c46ee906bda3a1e1f5026d4434 https://registry.npmjs.org/@openai/codex/-/codex-0.167.0-linux-x64.tgz /harness.tgz
          RUN tar --extract --gzip --file /harness.tgz --directory / package/vendor/x86_64-unknown-linux-musl/bin/codex package/vendor/x86_64-unknown-linux-musl/bin/codex-code-mode-host package/vendor/x86_64-unknown-linux-musl/codex-package.json package/vendor/x86_64-unknown-linux-musl/codex-path/rg
          FROM busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e AS codex-0.167.0-tree
          COPY --from=codex-0.167.0 /package/vendor/x86_64-unknown-linux-musl/ /out/harness/codex-0.167.0/
          RUN chmod 0444 /out/harness/codex-0.167.0/codex-package.json && chmod 0555 /out/harness /out/harness/codex-0.167.0 /out/harness/codex-0.167.0/bin /out/harness/codex-0.167.0/bin/codex /out/harness/codex-0.167.0/bin/codex-code-mode-host /out/harness/codex-0.167.0/codex-path /out/harness/codex-0.167.0/codex-path/rg
          FROM base
          COPY --link --from=codex-0.167.0-tree --chown=0:0 /out/ /
        /workspace/.codex/config.toml in full:
        model = "model-small"
        model_reasoning_effort = "high"
        model_provider = "pumpkin"
        # Nobody is there to approve a command, and the agent's sandbox confines Codex, whose own
        # sandbox needs `bwrap`, which its pin leaves out.
        approval_policy = "never"
        sandbox_mode = "danger-full-access"
        # A tool's shell reads no login profile, which could change the environment the command sets.
        allow_login_shell = false
        # Nothing reaches past the model route, the only way out of the sandbox.
        web_search = "disabled"

        # Codex's record of the run is the events it writes to its standard output, which the teardown
        # collects. The databases it makes in its home as it starts stay there, uncollected: `exec`
        # writes them no log, and its session, ephemeral, keeps nothing in them.

        # The model endpoint `sandboxd` serves, which needs no credential.
        [model_providers.pumpkin]
        # Any name but `OpenAI` keeps off the features Codex keeps for the provider of that name.
        name = "Pumpkin"
        base_url = "http://127.0.0.1:11235"
        wire_api = "responses"
        requires_openai_auth = false
        # A run of hours meets transient failures.
        request_max_retries = 100
        stream_max_retries = 100

        # The lead agent and as many spawned ones at most, each with one model request in flight at a
        # time: as many requests as the model route takes from a sandbox at once.
        [agents]
        max_concurrent_threads_per_session = 6

        [analytics]
        enabled = false

        [features]
        plugins = false
        apps = false
        image_generation = false

        [plugins._default]
        enabled = false
        model-small high native /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 ee811c2e99a8ede9e80570c6a373574bbcc8ee3e3fdae4a49588041615226810:
          model = "model-small"
          model_reasoning_effort = "high"
          model_provider = "pumpkin"
        route: responses model-small
        model-small high native /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 ee811c2e99a8ede9e80570c6a373574bbcc8ee3e3fdae4a49588041615226810:
          model = "model-small"
          model_reasoning_effort = "high"
          model_provider = "pumpkin"
        route: responses model-small
        model-large low native /home/agent:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/home/agent/.codex TMPDIR=/home/agent/.tmp
        cwd: /home/agent
        dir 0700 /home/agent/.codex
        dir 0700 /home/agent/.tmp
        file 0600 /home/agent/.codex/config.toml, sha256 2b61dbf17d70177420d3cff96e9d29b6ddfaeb5bb8d38f8835a55534329b790d:
          model = "model-large"
          model_reasoning_effort = "low"
          model_provider = "pumpkin"
        route: responses model-large
        gpt-6.1-terra xhigh functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 f51b61b2e69836ce741362bef47e261d767776b22c61c61f796cce5cd29b03ed:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "xhigh"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        glm-5.3 xhigh functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 cc7ba49f6cfbae4e13fe31a5d021536bb1342242722ce99d54b87dfd9edfcfa8:
          model = "glm-5.3"
          model_reasoning_effort = "xhigh"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 d07563ea73c2c8d733aaf17a4677eee0af89d2634ff454971107381341fe50b6:
          "glm-5.3" with 25 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses glm-5.3
        gpt-6.1-terra xhigh native /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 f51b61b2e69836ce741362bef47e261d767776b22c61c61f796cce5cd29b03ed:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "xhigh"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 0dd4b8f89941b13b64784e7a61353c85bf20921169f14c444c06032121588816:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: true
            tool_mode: "code_mode_only"
            apply_patch_tool_type: "freeform"
            multi_agent_version: "v2"
            experimental_supported_tools: ["send_user_message_async","clock"]
        route: responses gpt-6.1-terra
        glm-5.3 high native /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 065099ae03a8c39bbf8e99b92db9cf0c87c17dfb052c5fc334aef5d964722d88:
          model = "glm-5.3"
          model_reasoning_effort = "high"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 01a13e612272c3c5f25e8843d3a13c799190d28ed3543a5ec2f224edd8e84e23:
          "glm-5.3" with 22 fields:
            use_responses_lite: null
            tool_mode: null
            apply_patch_tool_type: "freeform"
            multi_agent_version: null
            experimental_supported_tools: []
        route: responses glm-5.3
        model-small xhigh native /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 03e451614854ed18c17d89d8cf9ca542345298cdf55f010fdc5a17c288c8b98a:
          model = "model-small"
          model_reasoning_effort = "xhigh"
          model_provider = "pumpkin"
        route: responses model-small
        vendor/"quoted"\model xhigh native /srv/work:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/srv/work/.codex TMPDIR=/srv/work/.tmp
        cwd: /srv/work
        dir 0700 /srv/work/.codex
        dir 0700 /srv/work/.tmp
        file 0600 /srv/work/.codex/config.toml, sha256 f99b52cc82c34b9cbb743ec23deb8079395f9f0e8941aea56ef222693e91bb3e:
          model = "vendor/\"quoted\"\\model"
          model_reasoning_effort = "xhigh"
          model_provider = "pumpkin"
        route: responses vendor/"quoted"\model
        glm-5.3 high functions /:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/.codex TMPDIR=/.tmp
        cwd: /
        dir 0700 /.codex
        dir 0700 /.tmp
        file 0600 /.codex/config.toml, sha256 5289f9135155be9da1bbb911d15af8bc6c0d5d478ee69758a7dd9e788443e6cd:
          model = "glm-5.3"
          model_reasoning_effort = "high"
          model_catalog_json = "/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /.codex/model-catalog.json, sha256 d07563ea73c2c8d733aaf17a4677eee0af89d2634ff454971107381341fe50b6:
          "glm-5.3" with 25 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses glm-5.3
        gpt-6.1-terra none functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 dd5594d733a50bfd35565060dad4259fc6f4d95f6d73275f6cbe02e201185878:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "none"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra minimal functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 f1f185ba1f3e8747990c0fb1a84d19d8ced0ffed89be049f7d91e7258478346d:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "minimal"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra low functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 a79a41fca71d88b22b21a1ce7fc2bf57ea377dddad9e60169e7bb500e5825c1b:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "low"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra medium functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 20a1f5d1eb22fed61eb4c212de1430dba0445c9838c4c2791a4106050e5b05a3:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "medium"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra high functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 11ad0da254120fd52f3e4130dc08568e864c1462fe116119448de51e831a06e0:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "high"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra xhigh functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 f51b61b2e69836ce741362bef47e261d767776b22c61c61f796cce5cd29b03ed:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "xhigh"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra max functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 8848e32e0cfada3f2fdb92b2aae155659290a6c2d019a34cfb92bfa4edd1ee4c:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "max"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        gpt-6.1-terra ultra functions /workspace:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        file 0600 /workspace/.codex/config.toml, sha256 6cd0aa5b1a7a9e698a13adb32d9e44e4ae53bb2fb759d89558c740b76e233733:
          model = "gpt-6.1-terra"
          model_reasoning_effort = "ultra"
          model_catalog_json = "/workspace/.codex/model-catalog.json"
          model_provider = "pumpkin"
        file 0600 /workspace/.codex/model-catalog.json, sha256 15b270cc4a43384e43f82433e7a00bd0b690b68849b431ec6bc5384363a1a611:
          "gpt-6.1-terra" with 50 fields:
            use_responses_lite: false
            tool_mode: "direct"
            apply_patch_tool_type: null
            multi_agent_version: "disabled"
            experimental_supported_tools: []
        route: responses gpt-6.1-terra
        "#]],
    );
}

#[test]
fn refusals() {
    let cases = [
        [
            "model-small",
            "extreme",
            "native",
            "/workspace",
            "",
            "60",
            "0",
        ],
        ["model-small", "high", "both", "/workspace", "", "60", "0"],
        [
            "gpt-6.1-sol",
            "high",
            "functions",
            "/workspace",
            "",
            "60",
            "0",
        ],
        [
            "model-small",
            "high",
            "native",
            "/pumpkin/home",
            "",
            "60",
            "0",
        ],
    ];
    let mut refused: Vec<String> = (cases.into_iter())
        .map(|case| {
            let case = case.map(str::to_owned);
            evaluated(ENTRY, &args(&case)).unwrap_err()
        })
        .collect();
    // A prompt that a task's `arg("prompt", str, default = None)` gives when no one passes one.
    let silent = ENTRY.replace("prompt = PROMPT,", "prompt = None,");
    let case = ["model-small", "high", "native", "/workspace", "", "60", "0"].map(str::to_owned);
    refused.push(evaluated(&silent, &args(&case)).unwrap_err());
    check(
        &refused.join("\n"),
        &expect![[r#"
            @pumpkin//harness/codex/codex.star:168:9: fail: `extreme` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra
                168 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:14:9
            @pumpkin//harness/codex/codex.star:185:9: fail: `both` is not a way to send Codex's tools: use one of native, functions
                185 |         fail("`{}` is not a way to send Codex's tools: use one of {}".format(tools, ", ".join(_TOOLS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:14:9
            @pumpkin//harness/codex/codex.star:182:13: fail: Codex's catalog entry for `gpt-6.1-sol` isn't carried, and sending its tools as functions needs it: use `gpt-6.1-terra` or `glm-5.3`
                182 |             fail(message.format(model, slugs))
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:14:9
            @pumpkin//harness/codex/codex.star:192:12: `/pumpkin/home/.codex` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                192 |     home = below + "/.codex"
                    |            ^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/codex/codex.star:170:9: fail: `None` is not a prompt: give a string, which may be empty
                170 |         fail("`None` is not a prompt: give a string, which may be empty")
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:14:9"#]],
    );
}

/// What `file` holds: a model catalog by its models' slugs, how many fields each has, and the
/// fields that set how Codex sends its tools; another file by its lines that name a model.
fn excerpt(file: &ir::File) -> String {
    if file.path.as_str().ends_with("/model-catalog.json") {
        return catalog(&file.content);
    }
    let mut shown = String::new();
    for line in (file.content.lines()).filter(|line| line.starts_with("model")) {
        let _ = writeln!(shown, "  {line}");
    }
    shown
}

/// The catalog `text` holds, as [`excerpt`] shows it.
fn catalog(text: &str) -> String {
    let catalog: Value = serde_json::from_str(text).unwrap();
    let fields = [
        "use_responses_lite",
        "tool_mode",
        "apply_patch_tool_type",
        "multi_agent_version",
        "experimental_supported_tools",
    ];
    let mut shown = String::new();
    for model in catalog["models"].as_array().unwrap() {
        let count = model.as_object().unwrap().len();
        let _ = writeln!(shown, "  {} with {count} fields:", model["slug"]);
        for field in fields {
            let _ = writeln!(shown, "    {field}: {}", model[field]);
        }
    }
    shown
}
