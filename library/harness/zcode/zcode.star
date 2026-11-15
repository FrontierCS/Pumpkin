"""ZCode, Z.ai's coding agent, as an agent's harness: `zcode` adds ZCode 0.16.9 to an agent's image
and writes the runtime of one unattended `zcode --prompt`, with the route to its model.

ZCode works in its workspace, and keeps its home (`HOME`), with its configuration, its sessions
and its other data (`ZCODE_DATA_BASE_DIR`), and its temporary files (`TMPDIR`) in a directory of
its own there. Its record of the run is the events it writes to its standard output as JSON
lines, which the teardown collects. It reaches the model only at the endpoint `sandboxd` serves,
`MODEL_URL`, on the Chat Completions API, through the one provider its provider file names, with
a stand-in key: the host holds the key, and the route passes no client's.
"""

load("@pumpkin//harness/node/node.star", "NODE", "node")

# Where `layer/Dockerfile` installs ZCode.
_ROOT = "/harness/zcode-0.16.9"

# The reasoning levels the provider file offers, each sent as the request's `reasoning_effort`.
_EFFORTS = ["low", "high", "max"]

# The one provider the provider file names. That file, the configuration and the command are
# those FrontierCS 2 runs ZCode with
# (https://github.com/FrontierCS/FrontierCS-2/blob/d247b3f15b73805e455ff93c6d043d84603ec341/candidates/zcode/launcher.py#L58-L146).
_PROVIDER = "pumpkin"

def zcode(
        base,
        *,
        model,
        effort,
        workspace,
        prompt,
        deadline_seconds,
        finalize_grace_seconds,
        context_tokens,
        output_tokens):
    """An agent role's `image`, `runtime` and `egress`, for ZCode on `model` at `effort`.

    ZCode runs once, as `zcode --prompt`, unattended, with its files readable by the agent's
    user alone.

    Args:
      base: the agent's image, which `image()` makes, to which the image adds ZCode and Node.
      model: the model's name, which the route names, and the one model ZCode calls.
      effort: how hard the model reasons, the request's `reasoning_effort`: one of `_EFFORTS`.
      workspace: the absolute path ZCode works in, which holds its home.
      prompt: the task ZCode works on, a string that is not empty.
      deadline_seconds: how long the work phase may last, a positive int.
      finalize_grace_seconds: how long ZCode may keep running after a final submission.
      context_tokens: how many tokens the model's context window holds, a positive int, by
        which ZCode decides when to compact the conversation.
      output_tokens: how many tokens one of the model's answers may hold at most, a positive
        int.

    Returns:
      A dict of `image`, `base` with ZCode added; `runtime`, the agent's runtime that runs it;
      and `egress`, the route to `model`, on the Chat Completions API, with the header ZCode
      sends that describes its client.
    """
    if effort not in _EFFORTS:
        fail("`{}` is not a reasoning level of ZCode's: use one of {}".format(effort, ", ".join(_EFFORTS)))
    if prompt == None or prompt == "":
        fail("ZCode runs only on a prompt: give a string that is not empty")
    if context_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(context_tokens))
    if output_tokens < 1:
        fail("`{}` is not a number of tokens: give a positive int".format(output_tokens))

    # A normalized absolute path ends in `/` only if it is `/`. The home is not `.zcode`, where
    # ZCode looks for a project's own configuration, plans and workflows.
    home = ("" if workspace == "/" else workspace) + "/.zcode-home"
    tmp = home + "/tmp"
    cli = home + "/.zcode/cli"
    providers = home + "/provider_config.json"
    config = {
        "permission": {"mode": "yolo"},
        # No plugin, no MCP server and no memory of earlier runs, but its skills, from no
        # directory.
        "plugins": {"enabled": False},
        "features": {"memory": False, "mcp": False},
        "memory": {"use": False},
        "skills": {"roots": []},
        # An hour without a byte of the model's stream, and for any request, before ZCode gives
        # up on it, so that a model that thinks long at a high effort is waited for.
        "modelStream": {"idleTimeoutMs": 3600000},
        "network": {"timeout": 3600000},
    }
    provider = {"schemaVersion": 1, "config": {
        "providerConfigRules": {"providerRules": [{
            "providerId": _PROVIDER,
            "providerName": "Pumpkin",
            "config": {
                "group": "standard-personal",
                # ZCode sends it as its bearer key; the route passes no `authorization` upstream.
                "access": {"type": "api-key", "apiKey": "pumpkin"},
                # ZCode sends its requests to `<base URL>/chat/completions`.
                "api": {"type": "openai-chat-completions", "baseUrl": MODEL_URL + "/v1"},
                "personalModelIds": [model],
                "modelOrder": [model],
            },
        }]},
        "modelConfigRules": {
            "providerModelRules": [{
                "providerId": _PROVIDER,
                "modelId": model,
                "config": {
                    "enabled": True,
                    "properties": {
                        "contextWindow": context_tokens,
                        "supportsToolCall": True,
                        "supportsJsonSchemaOutput": True,
                    },
                    # Each option, and the fields of a request that its value sets.
                    "optionSpecs": {
                        "reasoningLevel": {
                            "values": _EFFORTS,
                            "map": "{\"reasoning_effort\": reasoningLevel}",
                        },
                        "maxOutputTokens": {
                            "max": output_tokens,
                            "map": "{\"max_tokens\": maxOutputTokens}",
                        },
                    },
                },
            }],
            "manualProviderModelRules": [],
        },
        "defaultModelSelection": {
            "providerId": _PROVIDER,
            "modelId": model,
            "options": {"reasoningLevel": effort},
        },
    }}
    return {
        "image": image(
            "Dockerfile",
            context = "@pumpkin//harness/zcode/layer",
            contexts = {"base": base, "node": node()},
        ),
        "runtime": {"agent": {
            "command": {
                # ZCode on `node`, started directly (`node.star` says why). `--prompt` runs one
                # turn, unattended, on the prompt, which the
                # option takes whole, whatever it begins with, given as `--prompt=<text>`; and it
                # writes its events to its standard output as JSON lines, which the run's record
                # keeps. It asks no one for permission (`--mode yolo`), for the agent's sandbox
                # confines it, and offers no web search or fetch, which reach past the model.
                "argv": [
                    NODE,
                    _ROOT + "/zcode.cjs",
                    "--mode",
                    "yolo",
                    "--cwd",
                    workspace,
                    "--output-format",
                    "stream-json",
                    "--disallowed-tools",
                    "WebSearch,WebFetch",
                    "--prompt=" + prompt,
                ],
                "env": [
                    "HOME=" + home,
                    "TMPDIR=" + tmp,
                    "ZCODE_DATA_BASE_DIR=" + home,
                    "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE=" + providers,
                    "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=" + _ROOT + "/provider/zcode-builtin.json",
                ],
                "cwd": workspace,
            },
            "dirs": [{"path": path, "mode": "0700"} for path in [home, tmp, home + "/.zcode", cli]],
            "files": [
                {"path": cli + "/config.json", "mode": "0600", "content": json.encode(config)},
                {"path": providers, "mode": "0600", "content": json.encode(provider)},
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
