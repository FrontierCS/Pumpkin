//! `@pumpkin//harness/codex/codex.star`'s `codex()`: the agent role it gives for each set of
//! arguments that Pumpkin's examples and tests and the task repository give it, and the
//! arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use schema::ir;
use serde_json::Value;

use super::{check_refusals, check_roles};

/// The arguments `codex()` takes besides `SHARED`, with the Starlark of their values. A case
/// names its catalog entry by its slug, one of `models.json`'s, or as `none`.
fn own() -> [(&'static str, String); 2] {
    // On one line, so that the entry's lines number as the test's task writes them.
    let models: Value = serde_json::from_str(include_str!("codex/models.json")).unwrap();
    let models = models.to_string();
    assert!(
        !models.contains(r#"""""#),
        "no triple quote ends the string early"
    );
    // The string isn't raw, as the subset refuses a raw string with a backslash before a quote.
    let models = models.replace('\\', r"\\");
    let entry = format!(
        r#"{{entry["slug"]: entry for entry in json.decode("""{models}""")["models"]}}.get(arg("catalog_entry", str))"#
    );
    [
        ("tools", r#"arg("tools", str)"#.to_owned()),
        ("catalog_entry", entry),
    ]
}

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

/// Model, effort, workspace, prompt, deadline, grace, tools and catalog entry, from
/// `examples/codex`, `pumpkin`'s tests and the task repository; then other workspaces and prompts,
/// and the root as the workspace.
fn cases() -> Vec<[&'static str; 8]> {
    vec![
        // An empty prompt, which Codex takes, as `None` is the one it refuses.
        [
            "gpt-6.1-terra",
            "high",
            "/workspace",
            "",
            "60",
            "0",
            "native",
            "none",
        ],
        [
            "gpt-6.1-terra",
            "low",
            "/home/agent",
            "Solve it.",
            "60",
            "0",
            "native",
            "none",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/workspace",
            TASK_PROMPT,
            "18000",
            "60",
            "functions",
            "gpt-6.1-terra",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/workspace",
            TASK_PROMPT,
            "3600",
            "60",
            "native",
            "gpt-6.1-terra",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/srv/work",
            "Go.\n",
            "60",
            "0",
            "native",
            "none",
        ],
        [
            "gpt-6.1-terra",
            "high",
            "/",
            "Go.\n",
            "60",
            "0",
            "functions",
            "gpt-6.1-terra",
        ],
    ]
}

#[test]
#[expect(clippy::too_many_lines, reason = "most of it is the snapshot")]
fn roles() {
    let own = own();
    let own = own
        .each_ref()
        .map(|(keyword, value)| (*keyword, value.as_str()));
    check_roles(
        "codex",
        &own,
        &cases(),
        true,
        excerpt,
        &expect![[r#"
            gpt-6.1-terra high /workspace native none:
            argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
            env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
            cwd: /workspace
            dir 0700 /workspace/.codex
            dir 0700 /workspace/.tmp
            grant help before stdin
            file 0600 /workspace/.codex/config.toml:
              model = "gpt-6.1-terra"
              model_reasoning_effort = "high"
              model_provider = "pumpkin"
              base_url = "http://127.0.0.1:11235"
              max_concurrent_threads_per_session = 6
            route: responses gpt-6.1-terra
            request headers: originator user-agent session-id thread-id x-codex-beta-features x-codex-window-id x-codex-turn-metadata x-openai-internal-codex-responses-lite
            response headers: openai-model x-reasoning-included x-models-etag x-codex-turn-state x-codex-safety-buffering-enabled x-codex-safety-buffering-faster-model x-codex-credits-has-credits x-codex-credits-unlimited x-codex-credits-balance x-codex-promo-message x-codex-rate-limit-reached-type x-codex-active-limit x-oai-request-id cf-ray x-openai-authorization-error x-error-json x-codex-primary-used-percent x-codex-primary-window-minutes x-codex-primary-reset-at x-codex-secondary-used-percent x-codex-secondary-window-minutes x-codex-secondary-reset-at x-codex-limit-name
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/codex/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/codex/layer
            the agent's image: image 2
            gpt-6.1-terra low /home/agent native none:
            argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
            env: CODEX_HOME=/home/agent/.codex TMPDIR=/home/agent/.tmp
            cwd: /home/agent
            dir 0700 /home/agent/.codex
            dir 0700 /home/agent/.tmp
            grant help before stdin
            file 0600 /home/agent/.codex/config.toml:
              model = "gpt-6.1-terra"
              model_reasoning_effort = "low"
              model_provider = "pumpkin"
            route: responses gpt-6.1-terra
            gpt-6.1-terra xhigh /workspace functions gpt-6.1-terra:
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
            gpt-6.1-terra xhigh /workspace native gpt-6.1-terra:
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
            gpt-6.1-terra xhigh /srv/work native none:
            argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
            env: CODEX_HOME=/srv/work/.codex TMPDIR=/srv/work/.tmp
            cwd: /srv/work
            dir 0700 /srv/work/.codex
            dir 0700 /srv/work/.tmp
            grant help before stdin
            file 0600 /srv/work/.codex/config.toml:
              model = "gpt-6.1-terra"
              model_reasoning_effort = "xhigh"
              model_provider = "pumpkin"
            route: responses gpt-6.1-terra
            gpt-6.1-terra high / functions gpt-6.1-terra:
            argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
            env: CODEX_HOME=/.codex TMPDIR=/.tmp
            cwd: /
            dir 0700 /.codex
            dir 0700 /.tmp
            grant help before stdin
            file 0600 /.codex/config.toml:
              model = "gpt-6.1-terra"
              model_reasoning_effort = "high"
              model_catalog_json = "/.codex/model-catalog.json"
              model_provider = "pumpkin"
            file 0600 /.codex/model-catalog.json:
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
    let given = [
        "gpt-6.1-terra",
        "high",
        "/workspace",
        "",
        "60",
        "0",
        "functions",
        "gpt-6.1-terra",
    ];
    let own = own();
    let own = own
        .each_ref()
        .map(|(keyword, value)| (*keyword, value.as_str()));
    check_refusals(
        "codex",
        &own,
        &given,
        &[
            ("effort", "extreme"),
            ("tools", "both"),
            ("model", "gpt-6.2-sol"),
            ("catalog_entry", "none"),
            ("workspace", "/pumpkin/home"),
        ],
        &["prompt"],
        &expect![[r#"
            @pumpkin//harness/codex/codex.star:174:9: fail: `extreme` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra
                174 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:187:9: fail: `both` is not a way to send Codex's tools: use one of native, functions
                187 |         fail("`{}` is not a way to send Codex's tools: use one of {}".format(tools, ", ".join(_TOOLS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:179:9: fail: the catalog entry's slug is `gpt-6.1-terra`, so Codex wouldn't use it: set it to `gpt-6.2-sol`
                179 |         fail(message.format(catalog_entry.get("slug"), model))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:184:13: fail: sending Codex's tools as functions needs the model's catalog entry: give one
                184 |             fail("sending Codex's tools as functions needs the model's catalog entry: give one")
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:194:12: `/pumpkin/home/.codex` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                194 |     home = below + "/.codex"
                    |            ^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/codex/codex.star:176:9: fail: `None` is not a prompt: give a string, which may be empty
                176 |         fail("`None` is not a prompt: give a string, which may be empty")
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9"#]],
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
