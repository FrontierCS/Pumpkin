//! `@pumpkin//harness/codex/codex.star`'s `codex()`: the agent role it gives for each set of
//! arguments that Pumpkin's examples and tests give it, and the arguments it refuses.

use std::fmt::Write as _;

use expect_test::expect;
use schema::ir;

use super::{check_refusals, check_roles};

/// The arguments `codex()` takes besides `SHARED`, with the Starlark of their values.
const OWN: [(&str, &str); 2] = [
    ("tools", r#"arg("tools", str)"#),
    ("catalog_entry", r#"json.decode(arg("catalog_entry", str))"#),
];

/// Model, effort, workspace, prompt, deadline, grace, tools and catalog entry, from
/// `examples/codex` and `pumpkin`'s tests; then another workspace and prompt.
fn cases() -> Vec<[&'static str; 8]> {
    vec![
        // An empty prompt, which Codex takes, as `None` is the one it refuses.
        [
            "gpt-6.2-sol",
            "high",
            "/workspace",
            "",
            "60",
            "0",
            "native",
            "null",
        ],
        [
            "gpt-6.2-astra",
            "low",
            "/home/agent",
            "Solve it.",
            "60",
            "0",
            "native",
            "null",
        ],
        [
            "gpt-6.1-terra",
            "xhigh",
            "/srv/work",
            "Go.\n",
            "60",
            "0",
            "native",
            "null",
        ],
    ]
}

#[test]
fn roles() {
    check_roles(
        "codex",
        &OWN,
        &cases(),
        true,
        excerpt,
        &expect![[r#"
            gpt-6.2-sol high /workspace native null:
            argv: /harness/codex-0.172.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
            env: CODEX_HOME=/workspace/.codex TMPDIR=/workspace/.tmp
            cwd: /workspace
            dir 0700 /workspace/.codex
            dir 0700 /workspace/.tmp
            grant help before stdin
            file 0600 /workspace/.codex/config.toml:
              model = "gpt-6.2-sol"
              model_reasoning_effort = "high"
              model_provider = "pumpkin"
              base_url = "http://127.0.0.1:11235"
              max_concurrent_threads_per_session = 6
            route: responses gpt-6.2-sol
            request headers: originator user-agent session-id thread-id x-codex-beta-features x-codex-window-id x-codex-turn-metadata x-openai-internal-codex-responses-lite
            response headers: openai-model x-reasoning-included x-models-etag x-codex-turn-state x-codex-safety-buffering-enabled x-codex-safety-buffering-faster-model x-codex-credits-has-credits x-codex-credits-unlimited x-codex-credits-balance x-codex-promo-message x-codex-rate-limit-reached-type x-codex-active-limit x-oai-request-id cf-ray x-openai-authorization-error x-error-json x-codex-primary-used-percent x-codex-primary-window-minutes x-codex-primary-reset-at x-codex-secondary-used-percent x-codex-secondary-window-minutes x-codex-secondary-reset-at x-codex-limit-name
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/codex/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/codex/layer
            the agent's image: image 2
            gpt-6.2-astra low /home/agent native null:
            argv: /harness/codex-0.172.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
            env: CODEX_HOME=/home/agent/.codex TMPDIR=/home/agent/.tmp
            cwd: /home/agent
            dir 0700 /home/agent/.codex
            dir 0700 /home/agent/.tmp
            grant help before stdin
            file 0600 /home/agent/.codex/config.toml:
              model = "gpt-6.2-astra"
              model_reasoning_effort = "low"
              model_provider = "pumpkin"
            route: responses gpt-6.2-astra
            gpt-6.1-terra xhigh /srv/work native null:
            argv: /harness/codex-0.172.0/bin/codex exec --ephemeral --ignore-rules --skip-git-repo-check -
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
        "native",
        "null",
    ];
    check_refusals(
        "codex",
        &OWN,
        &given,
        &[
            ("effort", "extreme"),
            ("tools", "both"),
            ("tools", "functions"),
            ("catalog_entry", r#""gpt-6.1-terra""#),
            ("catalog_entry", r#"{"models": []}"#),
            ("catalog_entry", r#"{"slug": "gpt-6.2-sol"}"#),
            ("workspace", "/pumpkin/home"),
        ],
        &["prompt"],
        &expect![[r#"
            @pumpkin//harness/codex/codex.star:175:9: fail: `extreme` is not a reasoning effort: use one of none, minimal, low, medium, high, xhigh, max, ultra
                175 |         fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:195:9: fail: `both` is not a way to send Codex's tools: use one of native, functions
                195 |         fail("`{}` is not a way to send Codex's tools: use one of {}".format(tools, ", ".join(_TOOLS)))
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:192:13: fail: sending Codex's tools as functions needs the model's catalog entry: give one
                192 |             fail("sending Codex's tools as functions needs the model's catalog entry: give one")
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:181:13: fail: the catalog entry isn't a dict: give one entry, a dict, or `None`
                181 |             fail("the catalog entry isn't a dict: give one entry, a dict, or `None`")
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:184:13: fail: the catalog entry has no `slug`: give the one entry of `gpt-6.1-terra`, with its `slug`
                184 |             fail(message.format(model))
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:187:13: fail: the catalog entry's slug is `gpt-6.2-sol`, not the model's: set it to `gpt-6.1-terra`
                187 |             fail(message.format(catalog_entry["slug"], model))
                    |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9
            @pumpkin//harness/codex/codex.star:202:12: `/pumpkin/home/.codex` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                202 |     home = below + "/.codex"
                    |            ^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/codex/codex.star:177:9: fail: `None` is not a prompt: give a string, which may be empty
                177 |         fail("`None` is not a prompt: give a string, which may be empty")
                    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `codex()`, called at task.star:16:9"#]],
    );
}

/// What `file` holds, in the case at `index`: the lines that name a model, the lines in which the
/// cases' configurations differ, and in the first case also the lines that the harness fills from
/// Pumpkin's values.
fn excerpt(index: usize, file: &ir::File) -> String {
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
