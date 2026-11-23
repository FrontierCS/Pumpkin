//! `@pumpkin//harness/codex/codex.star`'s `codex()`: the agent role it gives for each set of
//! arguments that Pumpkin's examples and tests and the task repository give it, and the
//! arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use schema::ir;
use serde_json::Value;

use super::{check_refusals, check_roles};

/// The argument `codex()` takes besides `SHARED`, with the Starlark of its value.
const OWN: [(&str, &str); 1] = [("tools", r#"arg("tools", str)"#)];

/// A task's prompt, in full.
const TASK_PROMPT: &str = "Solve the evaluation task in /task/README.md.

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
            TASK_PROMPT,
            "18000",
            "60",
            "functions",
        ],
        [
            "glm-5.3",
            "xhigh",
            "/workspace",
            TASK_PROMPT,
            "3600",
            "60",
            "functions",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/workspace",
            TASK_PROMPT,
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
            TASK_PROMPT,
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
            file 0600 /workspace/.codex/config.toml:
              model = "model-small"
              model_reasoning_effort = "high"
              model_provider = "pumpkin"
              base_url = "http://127.0.0.1:11235"
              max_concurrent_threads_per_session = 6
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
            file 0600 /home/agent/.codex/config.toml:
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
            file 0600 /workspace/.codex/config.toml:
              model = "gpt-6.1-terra"
              model_reasoning_effort = "xhigh"
              model_catalog_json = "/workspace/.codex/model-catalog.json"
              model_provider = "pumpkin"
            file 0600 /workspace/.codex/model-catalog.json:
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
            file 0600 /workspace/.codex/config.toml:
              model = "glm-5.3"
              model_reasoning_effort = "xhigh"
              model_catalog_json = "/workspace/.codex/model-catalog.json"
              model_provider = "pumpkin"
            file 0600 /workspace/.codex/model-catalog.json:
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
            file 0600 /workspace/.codex/config.toml:
              model = "gpt-6.1-terra"
              model_reasoning_effort = "xhigh"
              model_catalog_json = "/workspace/.codex/model-catalog.json"
              model_provider = "pumpkin"
            file 0600 /workspace/.codex/model-catalog.json:
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
            file 0600 /workspace/.codex/config.toml:
              model = "glm-5.3"
              model_reasoning_effort = "high"
              model_catalog_json = "/workspace/.codex/model-catalog.json"
              model_provider = "pumpkin"
            file 0600 /workspace/.codex/model-catalog.json:
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
            file 0600 /workspace/.codex/config.toml:
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
            file 0600 /srv/work/.codex/config.toml:
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
            file 0600 /.codex/config.toml:
              model = "glm-5.3"
              model_reasoning_effort = "high"
              model_catalog_json = "/.codex/model-catalog.json"
              model_provider = "pumpkin"
            file 0600 /.codex/model-catalog.json:
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
/// fields each has, and the fields that set how Codex sends its tools; the configuration, and any
/// other file, by the lines that name a model, the lines in which the cases' configurations
/// differ, and in the first case also by the lines that the harness fills from Pumpkin's values.
fn excerpt(index: usize, file: &ir::File) -> String {
    if file.path.as_str().ends_with("/model-catalog.json") {
        return catalog(&file.content);
    }
    let filled = ["base_url", "max_concurrent_threads_per_session"];
    let shown = |line: &&str| {
        line.starts_with("model") || index == 0 && filled.iter().any(|key| line.starts_with(key))
    };
    let mut excerpt = String::new();
    for line in file.content.lines().filter(shown) {
        let _ = writeln!(excerpt, "  {line}");
    }
    excerpt
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
