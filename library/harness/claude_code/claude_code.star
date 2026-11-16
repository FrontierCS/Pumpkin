"""Claude Code, Anthropic's coding agent, as an agent's harness: `claude_code` adds Claude Code
2.1.296 to an agent's image and writes the runtime of one unattended `claude --print`, with the
route to its model.

Claude Code works in its workspace, and keeps its home (`HOME`), its configuration and sessions
(`CLAUDE_CONFIG_DIR`) and its temporary files (`TMPDIR`) in a directory of its own there. It
writes its last message, or its error, to its standard output. It reaches the model only at the
endpoint `sandboxd` serves, `MODEL_URL`, on the Messages API, with a stand-in key: the host holds
the key, and the route passes no client's. It reads every setting from its command and its
environment, none from a file.
"""

# Where `layer/Dockerfile` installs Claude Code, with the loader and the libraries it runs on.
_ROOT = "/harness/claude-code-2.1.296"

# The values of Claude Code's `--effort`, how hard the model reasons, as its `--help` lists them.
_EFFORTS = ["low", "medium", "high", "xhigh", "max"]

# Each variable that names a model Claude Code may call: the session's model, the default model,
# the small and fast model it gives light work to, the model each of its aliases (`fable`, `opus`,
# `sonnet`, `haiku`) names, which a sub-agent may ask for, sub-agents' own, and those of the
# sub-agents a workflow starts. Each names the one model the route allows, so that no request
# names another, which the route would refuse.
_MODEL_VARIABLES = [
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_WORKFLOW_SUBAGENT_MODEL",
]

# The request headers Claude Code sends that the route passes upstream, besides the Messages
# API's own: `anthropic-beta`, which names the betas the request's fields use (its context
# management, effort and thinking among them), and `user-agent` and `x-app`, which describe its
# client. The route reads a request's fields in any beta, and refuses each feature of the
# provider's own by its field.
_REQUEST_HEADERS = ["anthropic-beta", "user-agent", "x-app"]

def claude_code(
        base,
        *,
        model,
        effort,
        workspace,
        prompt,
        deadline_seconds,
        finalize_grace_seconds,
        max_output_tokens):
    """An agent role's `image`, `runtime` and `egress`, for Claude Code on `model` at `effort`.

    Claude Code runs once, as `claude --print`, on the prompt it reads on its standard input,
    with its files readable by the agent's user alone. It takes the arguments every harness takes,
    and `max_output_tokens`, in the sense `crates/library` gives them, and none of its own.

    Args:
      base: as every harness, and must have `bash`, which Claude Code's shell tool runs.
      model: as every harness; every model Claude Code calls.
      effort: as every harness; Claude Code's `--effort`: one of `_EFFORTS`.
      workspace: as every harness.
      prompt: as every harness; what Claude Code reads on its standard input, not empty.
      deadline_seconds: as every harness.
      finalize_grace_seconds: as every harness.
      max_output_tokens: as every harness that takes it; Claude Code's
        `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, which it lowers to the model's cap, 128,000 for a model
        it can't resolve (https://code.claude.com/docs/en/env-vars).

    Returns:
      A dict of `image`, `base` with Claude Code added; `runtime`, the agent's runtime that runs
      it; and `egress`, the route to `model`, on the Messages API, with the headers Claude Code
      sends.
    """
    if effort not in _EFFORTS:
        fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
    if prompt == None or prompt == "":
        fail("Claude Code runs only on a prompt: give a string that is not empty")
    if max_output_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))

    # A normalized absolute path ends in `/` only if it is `/`. The home is not `.claude`, where
    # Claude Code looks for a project's own settings, skills and agents.
    home = ("" if workspace == "/" else workspace) + "/.claude-code"
    config = home + "/config"
    tmp = home + "/tmp"
    env = [
        "HOME=" + home,
        "CLAUDE_CONFIG_DIR=" + config,
        "TMPDIR=" + tmp,
        # Claude Code sends its requests to `<base URL>/v1/messages`.
        "ANTHROPIC_BASE_URL=" + MODEL_URL,
        # Claude Code starts only with a credential, which it sends as `x-api-key`, and the route
        # replaces with the deployment's.
        "ANTHROPIC_API_KEY=pumpkin",
    ] + [name + "=" + model for name in _MODEL_VARIABLES] + [
        # Nothing reaches past the model route, the only way out of the sandbox: no update, no
        # telemetry, no error report, no other request that the work does not need.
        "DISABLE_AUTOUPDATER=1",
        "DISABLE_TELEMETRY=1",
        "DISABLE_ERROR_REPORTING=1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
        "CLAUDE_CODE_MAX_OUTPUT_TOKENS=" + str(max_output_tokens),
    ]
    return {
        "image": image("Dockerfile", context = "@pumpkin//harness/claude_code/layer", contexts = {"base": base}),
        "runtime": {"agent": {
            "command": {
                # `claude`, started directly, which `layer/Dockerfile` makes run on any base.
                # `--print` runs one turn, unattended, on the prompt it reads on its standard
                # input, which takes any text, and writes only its last message, or its error, to
                # its standard output (`--output-format text`,
                # https://code.claude.com/docs/en/cli-reference). It asks no one for permission, for
                # the agent's sandbox confines it; offers no web search or fetch, which only the
                # provider could run; starts no MCP server; and reads no settings file, the
                # image's user's or the workspace's (`--setting-sources ''`).
                "argv": [
                    _ROOT + "/claude",
                    "--print",
                    "--model",
                    model,
                    "--effort",
                    effort,
                    "--output-format",
                    "text",
                    "--dangerously-skip-permissions",
                    "--disallowedTools",
                    "WebSearch,WebFetch",
                    "--strict-mcp-config",
                    "--mcp-config",
                    json.encode({"mcpServers": {}}),
                    "--setting-sources",
                    "",
                ],
                "env": env,
                "cwd": workspace,
            },
            "dirs": [{"path": path, "mode": "0700"} for path in [home, config, tmp]],
            "stdin": prompt,
            "deadline_seconds": deadline_seconds,
            "finalize_grace_seconds": finalize_grace_seconds,
        }},
        "egress": {"model": [{
            "api": "messages",
            "models": [model],
            "request_headers": _REQUEST_HEADERS,
        }]},
    }
