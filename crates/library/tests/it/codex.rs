//! `@pumpkin//harness/codex/codex.star`'s `codex()`: the agent role it gives for each set of
//! arguments that Pumpkin's examples and tests and the task repository give it, and the
//! arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use schema::ir;
use serde_json::Value;

use super::{agent, args, check, check_refusals, check_roles, entry, evaluated};

/// The argument `codex()` takes besides `SHARED`, with the Starlark of its value.
const OWN: [(&str, &str); 1] = [("tools", r#"arg("tools", str)"#)];

/// The prompt of the task repository's `lib/fcs.star`.
const FCS_PROMPT: &str = "Solve the evaluation task in /frontier/task/README.md.

Before doing any task work, read that file completely. It is authoritative for the exact train
and final tar layouts, and for how to submit with `/.pumpkin/bin/sandboxctl`. Work only in
/workspace. Submit a tar for train verification, inspect its score and remaining calls with
`sandboxctl`, and iterate while useful. Do not write a replacement submission client.

When the answer is ready, create the final tar in the task-defined format, run
`/.pumpkin/bin/sandboxctl call final <tar> --arg finalize=true` exactly once as your last platform
action, then exit.
Web search is disabled; use only local task files and tools.
";

/// Model, effort, workspace, prompt, deadline, grace and tools, from `examples/codex`, `pumpkin`'s
/// tests and the task repository, on each model of the catalog; then other workspaces and
/// prompts, a model that needs escaping, and the root as the workspace.
fn cases() -> Vec<[&'static str; 7]> {
    vec![
        // An empty prompt, which Codex takes, as `None` is the one it refuses.
        ["model-small", "high", "/workspace", "", "60", "0", "native"],
        [
            "model-large",
            "low",
            "/home/agent",
            "Solve it.",
            "60",
            "0",
            "native",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/workspace",
            FCS_PROMPT,
            "18000",
            "60",
            "functions",
        ],
        [
            "glm-5.3",
            "xhigh",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
            "functions",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
            "native",
        ],
        [
            "glm-5.3",
            "high",
            "/workspace",
            "Go.\n",
            "3600",
            "60",
            "native",
        ],
        [
            "model-small",
            "xhigh",
            "/workspace",
            FCS_PROMPT,
            "3600",
            "60",
            "native",
        ],
        [
            r#"vendor/"quoted"\model"#,
            "xhigh",
            "/srv/work",
            "Go.\n",
            "60",
            "0",
            "native",
        ],
        ["glm-5.3", "high", "/", "Go.\n", "60", "0", "functions"],
    ]
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    check_roles(
        "codex",
        &OWN,
        &cases(),
        true,
        excerpt,
        &expect![[r#"
        model-small high /workspace native:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        grant help before stdin
        file 0600 /workspace/.codex/config.toml, sha256 8b9052558987320592d412af9c564e0809e5f8688dc1f409457077c3478c5db0:
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

        # No reasoning summary in the progress on the standard error: the model's answers hold them.
        hide_agent_reasoning = true

        # The databases Codex makes in its home as it starts stay there, uncollected: `exec` writes them
        # no log, and its session, ephemeral, keeps nothing in them.

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

        route: responses model-small
        request headers: originator user-agent session-id thread-id x-codex-beta-features x-codex-window-id x-codex-turn-metadata x-openai-internal-codex-responses-lite
        response headers: openai-model x-reasoning-included x-models-etag x-codex-turn-state x-codex-safety-buffering-enabled x-codex-safety-buffering-faster-model x-codex-credits-has-credits x-codex-credits-unlimited x-codex-credits-balance x-codex-promo-message x-codex-rate-limit-reached-type x-codex-active-limit x-oai-request-id cf-ray x-openai-authorization-error x-error-json x-codex-primary-used-percent x-codex-primary-window-minutes x-codex-primary-reset-at x-codex-secondary-used-percent x-codex-secondary-window-minutes x-codex-secondary-reset-at x-codex-limit-name
        image 1: //images's Dockerfile, in pumpkin/images
        image 2: @pumpkin//harness/codex/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/codex/layer
        the agent's image: image 2
        model-large low /home/agent native:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/home/agent/.codex TMPDIR=/home/agent/.tmp
        cwd: /home/agent
        dir 0700 /home/agent/.codex
        dir 0700 /home/agent/.tmp
        grant help before stdin
        file 0600 /home/agent/.codex/config.toml, sha256 4df80c6bdae7f455bd02fd17b9e36e63651851b279ea7615b7f1929ce579ad51:
          model = "model-large"
          model_reasoning_effort = "low"
          model_provider = "pumpkin"
        route: responses model-large
        gpt-6.1-terra xhigh /workspace functions:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        grant help before stdin
        file 0600 /workspace/.codex/config.toml, sha256 05e4d40af7a92a2f7dab9367df5b86cde8e1839dcef2350c22a2e6acaf4adc1e:
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
        glm-5.3 xhigh /workspace functions:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        grant help before stdin
        file 0600 /workspace/.codex/config.toml, sha256 26ba3c05bfc017461924b58f04cb4d3b7e31444656530c9b5a18d69622fdfc32:
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
        gpt-6.1-terra xhigh /workspace native:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        grant help before stdin
        file 0600 /workspace/.codex/config.toml, sha256 05e4d40af7a92a2f7dab9367df5b86cde8e1839dcef2350c22a2e6acaf4adc1e:
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
        glm-5.3 high /workspace native:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        grant help before stdin
        file 0600 /workspace/.codex/config.toml, sha256 6850fb3b65c5554f2cfc88d4e1f281f5b6244a70211d3805b5638c78d8d66ded:
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
        model-small xhigh /workspace native:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
        cwd: /workspace
        dir 0700 /workspace/.codex
        dir 0700 /workspace/.tmp
        grant help before stdin
        file 0600 /workspace/.codex/config.toml, sha256 0bddd09dc5bcb3cb9a03537325a5bc5ac6208006e284b692172b279132e5c748:
          model = "model-small"
          model_reasoning_effort = "xhigh"
          model_provider = "pumpkin"
        route: responses model-small
        vendor/"quoted"\model xhigh /srv/work native:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/srv/work/.codex TMPDIR=/srv/work/.tmp
        cwd: /srv/work
        dir 0700 /srv/work/.codex
        dir 0700 /srv/work/.tmp
        grant help before stdin
        file 0600 /srv/work/.codex/config.toml, sha256 043e1021813f1a6efcaa3769d68032cc1ef810fdbc284783ef6c8e1994a8312e:
          model = "vendor/\"quoted\"\\model"
          model_reasoning_effort = "xhigh"
          model_provider = "pumpkin"
        route: responses vendor/"quoted"\model
        glm-5.3 high / functions:
        argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
        env: CODEX_HOME=/.codex TMPDIR=/.tmp
        cwd: /
        dir 0700 /.codex
        dir 0700 /.tmp
        grant help before stdin
        file 0600 /.codex/config.toml, sha256 d0df3a4fc969805e18c18c7c1a66c5367bfd12f62464ef7e1726d293e73fdc09:
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
    "#]],
    );
}

// Each effort reaches `model_reasoning_effort`, and the model catalog stays what it is.
#[test]
fn efforts() {
    let mut shown = String::new();
    let mut catalog = None;
    for effort in [
        "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ] {
        let case = [
            "gpt-6.1-terra",
            effort,
            "/workspace",
            "Go.\n",
            "60",
            "0",
            "functions",
        ];
        let (workflow, _) = evaluated(&entry("codex", &OWN), &args(&OWN, &case)).unwrap();
        let (_, runtime) = agent(&workflow, &case);
        let [config, models] = [&runtime.files[0], &runtime.files[1]];
        let line = (config.content.lines()).find(|line| line.starts_with("model_reasoning_effort"));
        let _ = writeln!(shown, "{}", line.unwrap());
        let same = catalog.get_or_insert_with(|| models.content.clone());
        assert_eq!(*same, models.content);
    }
    check(
        &shown,
        &expect![[r#"
            model_reasoning_effort = "none"
            model_reasoning_effort = "minimal"
            model_reasoning_effort = "low"
            model_reasoning_effort = "medium"
            model_reasoning_effort = "high"
            model_reasoning_effort = "xhigh"
            model_reasoning_effort = "max"
            model_reasoning_effort = "ultra"
        "#]],
    );
}

#[test]
fn refusals() {
    let given = [
        "gpt-6.1-terra",
        "high",
        "/workspace",
        "",
        "60",
        "0",
        "functions",
    ];
    check_refusals(
        "codex",
        &OWN,
        &given,
        &[
            ("effort", "extreme"),
            ("tools", "both"),
            ("model", "gpt-6.1-sol"),
            ("workspace", "/pumpkin/home"),
        ],
        &["prompt"],
        &expect![[r#"
            @pumpkin//harness/codex/codex.star:170:9: fail: `extreme` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra
                170 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:15:9
            @pumpkin//harness/codex/codex.star:187:9: fail: `both` is not a way to send Codex's tools: use one of native, functions
                187 |         fail("`{}` is not a way to send Codex's tools: use one of {}".format(tools, ", ".join(_TOOLS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:15:9
            @pumpkin//harness/codex/codex.star:184:13: fail: Codex's catalog entry for `gpt-6.1-sol` isn't carried, and sending its tools as functions needs it: use `gpt-6.1-terra` or `glm-5.3`
                184 |             fail(message.format(model, slugs))
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:15:9
            @pumpkin//harness/codex/codex.star:194:12: `/pumpkin/home/.codex` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                194 |     home = below + "/.codex"
                    |            ^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/codex/codex.star:172:9: fail: `None` is not a prompt: give a string, which may be empty
                172 |         fail("`None` is not a prompt: give a string, which may be empty")
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:15:9"#]],
    );
}

/// What `file` holds, in the case at `index`: a model catalog by its models' slugs, how many
/// fields each has, and the fields that set how Codex sends its tools; the configuration of the
/// first case whole, as the cases' configurations differ only in the lines that name a model, and
/// those of the rest, and any other file, by those lines.
fn excerpt(index: usize, file: &ir::File) -> String {
    if file.path.as_str().ends_with("/model-catalog.json") {
        return catalog(&file.content);
    }
    if index == 0 {
        return format!("{}\n", file.content);
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
