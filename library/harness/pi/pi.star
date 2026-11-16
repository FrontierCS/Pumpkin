"""Pi, Earendil's coding agent (`@earendil-works/pi-coding-agent`), as an agent's harness: `pi`
adds Pi 1.2.0 to an agent's image and writes the runtime of one `pi --print`, with the route to its
model.

Pi works in its workspace, and keeps its home (`HOME`), with its configuration and its sessions in
`.pi/agent`, and its temporary files (`TMPDIR`) in a directory of its own there. It writes its last
message to its standard output and an error to its standard error. It reaches the model only at
the endpoint `sandboxd` serves, `MODEL_URL`, on the Chat Completions API, through the one provider
its model file names, with a stand-in key: the host holds the key, and the route passes no
client's.
"""

load("@pumpkin//harness/node/node.star", "NODE", "node")

# Where `layer/Dockerfile` installs Pi.
_ROOT = "/harness/pi-1.2.0"

# The thinking levels the model file offers, each sent as the request's `reasoning_effort`.
_EFFORTS = ["low", "medium", "high"]

# The one provider the model file names. That file, the settings and the command are those
# FrontierCS 2 runs Pi with
# (https://github.com/FrontierCS/FrontierCS-2/blob/d247b3f15b73805e455ff93c6d043d84603ec341/candidates/pi-gemini/launcher.py#L69-L175).
_PROVIDER = "pumpkin"

def pi(
        base,
        *,
        model,
        effort,
        workspace,
        prompt,
        deadline_seconds,
        finalize_grace_seconds,
        max_output_tokens,
        context_tokens):
    """An agent role's `image` and `runtime`, for Pi on `model` at `effort`.

    Pi runs once, as `pi --print`, unattended, with its files readable by the agent's user alone.
    It takes the arguments every harness takes, `max_output_tokens` and `context_tokens`, in the
    sense `crates/library` gives them, and none of its own.

    Args:
      base: as every harness; the image adds Node too.
      model: as every harness.
      effort: as every harness; the request's `reasoning_effort`: one of `_EFFORTS`.
      workspace: as every harness.
      prompt: as every harness; not empty, and not beginning with `@`.
      deadline_seconds: as every harness.
      finalize_grace_seconds: as every harness.
      max_output_tokens: as every harness that takes it; the request's `max_completion_tokens`.
      context_tokens: as every harness that takes it.

    Returns:
      A dict of `image`, `base` with Pi added; and `runtime`, the agent's runtime that runs it,
      with its `egress`, the route to `model`, on the Chat Completions API, with the header Pi
      sends that describes its client.
    """
    if effort not in _EFFORTS:
        fail("`{}` is not a thinking level of Pi's: use one of {}".format(effort, ", ".join(_EFFORTS)))
    if prompt == None or prompt == "":
        fail("Pi runs only on a prompt: give a string that is not empty")

    # Pi reads an argument that begins with `@` as a file to attach, even after `--`.
    if prompt.startswith("@"):
        fail("Pi reads a prompt that begins with `@` as a file's path: begin it with another character")
    if context_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
    if max_output_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))

    # A normalized absolute path ends in `/` only if it is `/`. The home is not `.pi`, where Pi
    # looks for a project's own configuration. Pi keeps its configuration and its sessions in
    # `.pi/agent` below its home.
    home = ("" if workspace == "/" else workspace) + "/.pi-home"
    tmp = home + "/tmp"
    agent = home + "/.pi/agent"
    models = {"providers": {_PROVIDER: {
        # Pi sends its requests to `<base URL>/chat/completions`.
        "baseUrl": MODEL_URL + "/v1",
        "api": "openai-completions",
        # Pi sends it as its bearer key; the route passes no `authorization` upstream.
        "apiKey": "pumpkin",
        "models": [{
            "id": model,
            "reasoning": True,
            "thinkingLevelMap": {level: level for level in _EFFORTS},
            "contextWindow": context_tokens,
            "maxTokens": max_output_tokens,
            # The request's form, which Pi would otherwise guess from the endpoint's URL: no
            # `store` and the instructions as a `system` message, which every Chat Completions
            # upstream takes.
            "compat": {
                "supportsStore": False,
                "supportsDeveloperRole": False,
                "supportsReasoningEffort": True,
                "thinkingFormat": "openai",
                "maxTokensField": "max_completion_tokens",
            },
        }],
    }}}
    settings = {
        # An hour without a byte of the model's stream before Pi gives up on a request, so that a
        # model that thinks long at a high effort is waited for.
        "httpIdleTimeoutMs": 3600000,
        "retry": {"maxRetries": 16},
    }
    return {
        "image": image(
            "Dockerfile",
            context = "@pumpkin//harness/pi/layer",
            contexts = {"base": base, "node": node()},
        ),
        "runtime": {"agent": {
            "command": {
                # Pi on `node`, started directly (`node.star` says why). `--print` runs one turn on
                # the prompt, which follows `--` so that it may begin with `-`, and writes only its
                # last message to its standard output, or an error to its standard error and exits
                # nonzero
                # (https://github.com/earendil-works/pi/blob/06a466935536f8c9bcacb2d5efc0fc021be1bdff/packages/coding-agent/docs/cli-integration.md?plain=1#L22-L30).
                # It reads no project's own configuration (`--no-approve`), fetches nothing but the
                # model's answers (`--offline`), and loads no extension, MCP server, prompt
                # template, theme or skill. Its tools are those it starts with but `grep` and
                # `find`, which would download their programs.
                "argv": [
                    NODE,
                    _ROOT + "/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js",
                    "--print",
                    "--offline",
                    "--no-approve",
                    "--provider",
                    _PROVIDER,
                    "--model",
                    model,
                    "--thinking",
                    effort,
                    "--tools",
                    "read,bash,edit,write",
                    "--no-extensions",
                    "--no-mcp",
                    "--no-prompt-templates",
                    "--no-themes",
                    "--no-skills",
                    "--",
                    prompt,
                ],
                # No install report, and no header that names Pi to the model's provider.
                "env": ["HOME=" + home, "TMPDIR=" + tmp, "PI_TELEMETRY=0"],
                "cwd": workspace,
            },
            "dirs": [{"path": path, "mode": "0700"} for path in [home, tmp, home + "/.pi", agent]],
            "files": [
                {"path": agent + "/models.json", "mode": "0600", "content": json.encode(models)},
                {"path": agent + "/settings.json", "mode": "0600", "content": json.encode(settings)},
            ],
            "deadline_seconds": deadline_seconds,
            "finalize_grace_seconds": finalize_grace_seconds,
            "egress": {"model": [{
                "api": "chat",
                "models": [model],
                "request_headers": ["user-agent"],
            }]},
        }},
    }
