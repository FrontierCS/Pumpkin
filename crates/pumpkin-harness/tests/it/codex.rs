//! The agent's runtime that runs Codex: for an upstream that forwards only function tools, and
//! for one that forwards Codex's own forms; and the inputs the adapter refuses.

#![expect(
    clippy::unwrap_used,
    reason = "a test helper fails the test that calls it"
)]

use std::fmt::Write as _;

use expect_test::{Expect, expect};
use pumpkin_harness::codex::{Codex, ReasoningEffort, Tools};
use pumpkin_schema::ir::{AbsolutePath, AgentRuntime, EnvVar, ModelName};
use serde_json::Value;

fn check(actual: &str, expect: &Expect) {
    expect.assert_eq(actual);
}

/// Codex working in `/workspace` on `gpt-6.1-terra`, through an upstream that forwards only
/// function tools: its catalog entry, of which the snapshot shows the fields that set how Codex
/// sends its tools.
#[test]
fn functions() {
    let codex = codex("/workspace", "gpt-6.1-terra", Tools::Functions).unwrap();
    check(
        &show(&codex.runtime("Solve the task in /task/README.md.\n")),
        &expect![[r#"
            argv: /harness/codex-0.167.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check --json -
            env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
            cwd: /workspace
            stdin: Some("Solve the task in /task/README.md.\n")
            dir 0700 /workspace/.codex
            dir 0700 /workspace/.tmp
            file 0600 /workspace/.codex/config.toml:
            model = "gpt-6.1-terra"
            model_reasoning_effort = "xhigh"
            model_catalog_json = "/workspace/.codex/model-catalog.json"
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
            file 0600 /workspace/.codex/model-catalog.json:
            "gpt-6.1-terra" with 50 fields:
              use_responses_lite: false
              tool_mode: "direct"
              apply_patch_tool_type: null
              multi_agent_version: "disabled"
              experimental_supported_tools: []
        "#]],
    );
}

/// Codex through an upstream that forwards Codex's own forms, which reads no catalog but its
/// own: on a model whose name its configuration quotes, in a workspace below `/`'s. The
/// snapshot shows the files and the configuration's lines about the model, the rest being
/// [`functions`]'s.
#[test]
fn native() {
    let codex = codex("/srv/work", r#"vendor/"quoted"\model"#, Tools::Native).unwrap();
    let runtime = codex.runtime("Go.\n");
    let mut shown = String::new();
    for file in &runtime.files {
        let _ = writeln!(shown, "file {} {}:", file.mode, file.path);
        for line in (file.content.lines()).filter(|line| line.starts_with("model")) {
            let _ = writeln!(shown, "{line}");
        }
    }
    check(
        &shown,
        &expect![[r#"
            file 0600 /srv/work/.codex/config.toml:
            model = "vendor/\"quoted\"\\model"
            model_reasoning_effort = "xhigh"
            model_provider = "pumpkin"
        "#]],
    );
}

/// Codex through an upstream that forwards Codex's own forms, on a model whose catalog entry
/// the crate carries and Codex's own catalog lacks: the entry goes in its home.
#[test]
fn native_carried() {
    let codex = codex("/workspace", "glm-5.3", Tools::Native).unwrap();
    let runtime = codex.runtime("Go.\n");
    let mut shown = String::new();
    for file in &runtime.files {
        let _ = writeln!(shown, "file {} {}:", file.mode, file.path);
        if file.path.as_str().ends_with("/model-catalog.json") {
            shown.push_str(&catalog(&file.content));
        } else {
            for line in (file.content.lines()).filter(|line| line.starts_with("model")) {
                let _ = writeln!(shown, "{line}");
            }
        }
    }
    check(
        &shown,
        &expect![[r#"
            file 0600 /workspace/.codex/config.toml:
            model = "glm-5.3"
            model_reasoning_effort = "xhigh"
            model_catalog_json = "/workspace/.codex/model-catalog.json"
            model_provider = "pumpkin"
            file 0600 /workspace/.codex/model-catalog.json:
            "glm-5.3" with 22 fields:
              use_responses_lite: null
              tool_mode: null
              apply_patch_tool_type: "freeform"
              multi_agent_version: null
              experimental_supported_tools: []
        "#]],
    );
}

/// What the adapter refuses: a workspace whose directories a workflow can't put, a model whose
/// catalog entry sending tools as functions needs and the crate doesn't carry, and an effort
/// Codex doesn't know.
#[test]
fn refusals() {
    let errors = [
        codex("/pumpkin/home", "gpt-6.1-terra", Tools::Native).map(|_| ()),
        codex("/workspace", "gpt-6.1-sol", Tools::Functions).map(|_| ()),
        ReasoningEffort::try_from("extreme").map(|_| ()),
    ];
    let errors: Vec<String> = errors.into_iter().map(|error| error.unwrap_err()).collect();
    check(
        &errors.join("\n"),
        &expect![[r#"
            `/pumpkin/home/.codex` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
            Codex's catalog entry for `gpt-6.1-sol` isn't carried, and sending its tools as functions needs it: use `gpt-6.1-terra` or `glm-5.3`
            `extreme` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra"#]],
    );
}

/// Codex working in `workspace` on `model` at `xhigh`, sending its tools as `tools` says.
fn codex(workspace: &str, model: &str, tools: Tools) -> Result<Codex, String> {
    let workspace = AbsolutePath::try_from(workspace.to_owned()).unwrap();
    let model = ModelName::try_from(model.to_owned()).unwrap();
    Codex::new(workspace, &model, ReasoningEffort::XHigh, tools)
}

/// `runtime`, a line for each part; a model catalog by its models' slugs, the fields that set
/// how Codex sends its tools, and how many fields each has.
fn show(runtime: &AgentRuntime) -> String {
    let AgentRuntime {
        command,
        dirs,
        files,
        stdin,
    } = runtime;
    let argv: Vec<String> = command.argv.clone().into();
    let env: Vec<EnvVar> = command.env.clone().into();
    let env: Vec<String> = env.into_iter().map(String::from).collect();
    let mut shown = format!(
        "argv: {}\nenv: {}\ncwd: {}\nstdin: {stdin:?}\n",
        argv.join(" "),
        env.join(" "),
        command.cwd,
    );
    for dir in dirs {
        let _ = writeln!(shown, "dir {} {}", dir.mode, dir.path);
    }
    for file in files {
        let content = if file.path.as_str().ends_with("/model-catalog.json") {
            catalog(&file.content)
        } else {
            file.content.clone()
        };
        let _ = write!(shown, "file {} {}:\n{content}", file.mode, file.path);
    }
    shown
}

/// The catalog `text` holds, as [`show`] shows it.
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
        let _ = writeln!(shown, "{} with {count} fields:", model["slug"]);
        for field in fields {
            let _ = writeln!(shown, "  {field}: {}", model[field]);
        }
    }
    shown
}
