"""Qwen Code, Qwen's coding agent, as an agent's harness: `qwen_code` adds Qwen Code 0.25.0 to an
agent's image and writes the runtime of one unattended `qwen --prompt`, with the route to its
model.

Qwen Code works in its workspace, and keeps its home (`HOME`), with its settings, its sessions and
its other state (`QWEN_HOME`), and its temporary files (`TMPDIR`) in a directory of its own there.
It writes its last message to its standard output and an error to its standard error. It reaches
the model only at the endpoint `sandboxd` serves, `MODEL_URL`, on the Chat Completions API,
through the one provider its settings name, with a stand-in key: the host holds the key, and the
route passes no client's.
"""

load("@pumpkin//harness/node/node.star", "NODE", "node")

# Where `layer/Dockerfile` installs Qwen Code.
_ROOT = "/harness/qwen-code-0.25.0"

# The reasoning levels Qwen Code knows, each sent as the request's `reasoning_effort`.
_EFFORTS = ["low", "medium", "high", "xhigh", "max"]

# The variable that holds the stand-in key, which the settings name.
_KEY = "PUMPKIN_MODEL_KEY"

def qwen_code(
        base,
        *,
        model,
        effort,
        workspace,
        prompt,
        deadline_seconds,
        finalize_grace_seconds,
        max_output_tokens,
        context_tokens,
        extra_body):
    """An agent role's `image`, `runtime` and `egress`, for Qwen Code on `model` at `effort`.

    Qwen Code runs once, as `qwen --prompt`, unattended, with its files readable by the agent's
    user alone. It takes the arguments every harness takes, `max_output_tokens` and
    `context_tokens`, in the sense `crates/library` gives them, and `extra_body`, its own.

    Args:
      base: as every harness, and must have `bash`, which Qwen Code's shell tool runs; the image
        adds Node too.
      model: as every harness.
      effort: as every harness; the request's `reasoning_effort`: one of `_EFFORTS`.
      workspace: as every harness.
      prompt: as every harness; not empty.
      deadline_seconds: as every harness.
      finalize_grace_seconds: as every harness.
      max_output_tokens: as every harness that takes it; the request's `max_tokens`.
      context_tokens: as every harness that takes it.
      extra_body: the other fields each request's body holds, a dict, such as a sampling
        temperature; `effort` and `max_output_tokens` give `reasoning_effort` and `max_tokens`.

    Returns:
      A dict of `image`, `base` with Qwen Code added; `runtime`, the agent's runtime that runs
      it; and `egress`, the route to `model`, on the Chat Completions API, with the header Qwen
      Code sends that describes its client.
    """
    if effort not in _EFFORTS:
        fail("`{}` is not a reasoning level of Qwen Code's: use one of {}".format(effort, ", ".join(_EFFORTS)))
    if prompt == None or prompt == "":
        fail("Qwen Code runs only on a prompt: give a string that is not empty")
    if context_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
    if max_output_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(max_output_tokens))

    # A normalized absolute path ends in `/` only if it is `/`. The home is not `.qwen`, where
    # Qwen Code looks for a project's own settings, skills and agents.
    home = ("" if workspace == "/" else workspace) + "/.qwen-home"
    tmp = home + "/tmp"
    state = home + "/qwen"
    body = extra_body | {"reasoning_effort": effort, "max_tokens": max_output_tokens}

    # The settings, which, with the command and the environment, are those FrontierCS 2 runs Qwen
    # Code with, but for the usage statistics, the update check and the catalog
    # (https://github.com/FrontierCS/FrontierCS-2/blob/d247b3f15b73805e455ff93c6d043d84603ec341/candidates/qwen-code/launcher.py#L63-L154).
    settings = {
        "$version": 4,
        "security": {"auth": {"selectedType": "openai"}},
        "model": {"name": model},
        "modelProviders": {"openai": [{
            "id": model,
            "envKey": _KEY,
            # Qwen Code sends its requests to `<base URL>/chat/completions`.
            "baseUrl": MODEL_URL + "/v1",
            "wireApi": "chat-completions",
            "generationConfig": {
                "contextWindowSize": context_tokens,
                # An hour for any request, and without a byte of the model's stream, before Qwen
                # Code gives up on it, so that a model that thinks long at a high effort is
                # waited for.
                "timeout": 3600000,
                "streamIdleTimeoutMs": 3600000,
                "maxRetries": 16,
                # Merged into each request's body last, so that Qwen Code's own estimate of the
                # room left in the context window doesn't lower `max_tokens`.
                "extra_body": body,
            },
        }]},
        # No skill but the user's, of which the home holds none, no MCP server, and no memory
        # of earlier runs.
        "skills": {"disabledLevels": ["project", "extension", "bundled"]},
        "mcpServers": {},
        "memory": {"enableManagedAutoMemory": False, "enableManagedAutoDream": False, "enableAutoSkill": False},
        # Nothing sent but to the model: no usage statistics, no telemetry, no check for a newer
        # release, and no web search or fetch.
        "privacy": {"usageStatisticsEnabled": False},
        "telemetry": {"enabled": False},
        "general": {"enableAutoUpdate": False},
        # A shell command runs until it ends, not two minutes, Qwen Code's default.
        "tools": {"disabled": ["web_fetch", "web_search"], "shell": {"defaultTimeoutMs": 0}},
    }
    return {
        "image": image(
            "Dockerfile",
            context = "@pumpkin//harness/qwen_code/layer",
            contexts = {"base": base, "node": node()},
        ),
        "runtime": {"agent": {
            "command": {
                # Qwen Code on `node`, started directly (`node.star` says why). `--prompt` runs
                # one turn, unattended, on the prompt, which the option takes whole, whatever it
                # begins with, given as `--prompt=<text>`; and `text` writes only its last message
                # to its standard output, and an error to its standard error
                # (https://github.com/QwenLM/qwen-code/blob/6788c035698a0ada471c958d1e789e96c6cddd9b/packages/cli/src/nonInteractive/io/JsonOutputAdapter.ts#L123-L128).
                # It asks no one for permission (`--approval-mode yolo`), for the agent's sandbox
                # confines it.
                "argv": [
                    NODE,
                    _ROOT + "/cli-entry.js",
                    "--auth-type",
                    "openai",
                    "--model",
                    model,
                    "--approval-mode",
                    "yolo",
                    "--output-format",
                    "text",
                    "--prompt=" + prompt,
                ],
                "env": [
                    "HOME=" + home,
                    "TMPDIR=" + tmp,
                    "QWEN_HOME=" + state,
                    # Qwen Code sends it as its bearer key; the route passes no `authorization`
                    # upstream.
                    _KEY + "=pumpkin",
                    # No end to a stream that still flows, which Qwen Code cuts after 15 minutes.
                    "QWEN_STREAM_MAX_LIFETIME_MS=0",
                    # The catalog of models Qwen Code is built with, not one it would fetch.
                    "QWEN_CODE_MODELS_DEV_REFRESH=off",
                ],
                "cwd": workspace,
            },
            "dirs": [{"path": path, "mode": "0700"} for path in [home, tmp, state]],
            "files": [
                {"path": state + "/settings.json", "mode": "0600", "content": json.encode(settings)},
            ],
            "deadline_seconds": deadline_seconds,
            "finalize_grace_seconds": finalize_grace_seconds,
        }},
        "egress": {"model": [{
            "api": "chat",
            "models": [model],
            "request_headers": ["user-agent"],
        }]},
    }
