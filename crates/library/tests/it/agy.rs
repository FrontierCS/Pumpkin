//! `@pumpkin//harness/agy/agy.star`'s `agy()`: the agent role it gives for each set of
//! arguments, and the arguments it refuses.

use expect_test::expect;

use super::{TASK_PROMPT, check_refusals, check_roles};

/// `agy()` takes no argument besides `SHARED`.
const OWN: [(&str, &str); 0] = [];

#[test]
fn roles() {
    let cases = [
        [
            "gemini-4-hydrogen",
            "high",
            "/workspace",
            TASK_PROMPT,
            "18000",
            "60",
        ],
        [
            "gemini-4-neon",
            "xhigh",
            r#"/srv/my "work" dir"#,
            "-leading dash\nsecond line, \"quoted\"\n",
            "60",
            "0",
        ],
        ["gemini-4-carbon", "low", "/", "Go.", "60", "0"],
    ];
    // The settings' whole text once: no case changes it.
    check_roles(
        "agy",
        &OWN,
        &cases,
        false,
        |index, file| {
            if index == 0 {
                format!("{}\n", file.content)
            } else {
                String::new()
            }
        },
        &expect![[r#"
            gemini-4-hydrogen high /workspace :
            argv: /harness/agy-1.5.16/antigravity "--print=Solve the evaluation task in /task/README.md.\n\nWhen the answer is ready, run `/.pumpkin/bin/sandboxctl call final <tar> --arg finalize=true`.\n" --model gemini-4-hydrogen-high --output-format stream-json --dangerously-skip-permissions --disable-slash-commands --log-file /workspace/.agy-home/agy.log
            env: HOME=/workspace/.agy-home TMPDIR=/workspace/.agy-home/tmp GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:11235 GEMINI_API_KEY=pumpkin AGY_CLI_DISABLE_AUTO_UPDATE=1
            cwd: /workspace
            dir 0700 /workspace/.agy-home
            dir 0700 /workspace/.agy-home/tmp
            dir 0700 /workspace/.agy-home/.gemini
            dir 0700 /workspace/.agy-home/.gemini/antigravity-cli
            file 0600 /workspace/.agy-home/.gemini/antigravity-cli/settings.json:
            {"allowNonWorkspaceAccess":true,"artifactReviewPolicy":"always-proceed","enableTelemetry":false,"enableTerminalSandbox":false,"modelProvider":"gemini","notifications":false,"toolPermission":"always-proceed"}
            route: generate_content gemini-4-hydrogen
            request headers: none
            response headers: none
            image 1: //images's Dockerfile, in pumpkin/images
            image 2: @pumpkin//harness/agy/layer's Dockerfile, image 1 as base, in pumpkin-library/harness/agy/layer
            the agent's image: image 2
            gemini-4-neon xhigh /srv/my "work" dir :
            argv: /harness/agy-1.5.16/antigravity "--print=-leading dash\nsecond line, \"quoted\"\n" --model gemini-4-neon-xhigh --output-format stream-json --dangerously-skip-permissions --disable-slash-commands --log-file "/srv/my \"work\" dir/.agy-home/agy.log"
            env: "HOME=/srv/my \"work\" dir/.agy-home" "TMPDIR=/srv/my \"work\" dir/.agy-home/tmp" GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:11235 GEMINI_API_KEY=pumpkin AGY_CLI_DISABLE_AUTO_UPDATE=1
            cwd: /srv/my "work" dir
            dir 0700 /srv/my "work" dir/.agy-home
            dir 0700 /srv/my "work" dir/.agy-home/tmp
            dir 0700 /srv/my "work" dir/.agy-home/.gemini
            dir 0700 /srv/my "work" dir/.agy-home/.gemini/antigravity-cli
            file 0600 /srv/my "work" dir/.agy-home/.gemini/antigravity-cli/settings.json:
            route: generate_content gemini-4-neon
            gemini-4-carbon low / :
            argv: /harness/agy-1.5.16/antigravity --print=Go. --model gemini-4-carbon-low --output-format stream-json --dangerously-skip-permissions --disable-slash-commands --log-file /.agy-home/agy.log
            env: HOME=/.agy-home TMPDIR=/.agy-home/tmp GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:11235 GEMINI_API_KEY=pumpkin AGY_CLI_DISABLE_AUTO_UPDATE=1
            cwd: /
            dir 0700 /.agy-home
            dir 0700 /.agy-home/tmp
            dir 0700 /.agy-home/.gemini
            dir 0700 /.agy-home/.gemini/antigravity-cli
            file 0600 /.agy-home/.gemini/antigravity-cli/settings.json:
            route: generate_content gemini-4-carbon
        "#]],
    );
}

#[test]
fn refusals() {
    let given = ["gemini-4-argon", "high", "/workspace", "Go.", "60", "0"];
    check_refusals(
        "agy",
        &OWN,
        &given,
        &[
            ("model", "gemini-3.8-flash"),
            ("effort", "xhigh"),
            ("effort", "High"),
            ("workspace", "/pumpkin/home"),
            ("prompt", ""),
        ],
        &["prompt"],
        &expect![[r#"
            @pumpkin//harness/agy/agy.star:65:9: fail: `gemini-3.8-flash` is not a model that agy starts on: use one of gemini-4-hydrogen, gemini-4-neon, gemini-4-carbon, gemini-4-argon
                65 |         fail("`{}` is not a model that agy starts on: use one of {}".format(model, ", ".join(_EFFORTS.keys())))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `agy()`, called at task.star:14:9
            @pumpkin//harness/agy/agy.star:67:9: fail: `xhigh` is not a thinking level agy runs `gemini-4-argon` at: use one of low, medium, high
                67 |         fail("`{}` is not a thinking level agy runs `{}` at: use one of {}".format(effort, model, ", ".join(_EFFORTS[model])))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `agy()`, called at task.star:14:9
            @pumpkin//harness/agy/agy.star:67:9: fail: `High` is not a thinking level agy runs `gemini-4-argon` at: use one of low, medium, high
                67 |         fail("`{}` is not a thinking level agy runs `{}` at: use one of {}".format(effort, model, ", ".join(_EFFORTS[model])))
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `agy()`, called at task.star:14:9
            @pumpkin//harness/agy/agy.star:72:13: `/pumpkin/home/.agy-home` is in `/pumpkin`, which holds Pumpkin's own files: name a path outside it
                72 |     home = ("" if workspace == "/" else workspace) + "/.agy-home"
                   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                the IR's /roles/0/runtime/agent/dirs/0/path
            @pumpkin//harness/agy/agy.star:69:9: fail: agy runs only on a prompt: give a string that is not empty
                69 |         fail("agy runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `agy()`, called at task.star:14:9
            @pumpkin//harness/agy/agy.star:69:9: fail: agy runs only on a prompt: give a string that is not empty
                69 |         fail("agy runs only on a prompt: give a string that is not empty")
                   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                in `agy()`, called at task.star:14:9"#]],
    );
}
