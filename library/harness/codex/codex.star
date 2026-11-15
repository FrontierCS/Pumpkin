"""Codex, OpenAI's coding agent, as an agent's harness: `codex` adds Codex 0.167.0 to an agent's
image and writes the runtime of one unattended `codex exec`, with the route to its model.

Codex works in its workspace, and keeps its home (`CODEX_HOME`), with its configuration and its
databases, and its temporary files (`TMPDIR`) in two directories of their own there. Its record of
the run is the events it writes to its standard output as JSON lines, which the teardown collects.
It reaches the model only at the endpoint `sandboxd` serves, `MODEL_URL`, on the Responses API,
with no credential: the host holds the key.
"""

load("@pumpkin//harness/codex/catalog.star", "CATALOG")

# Where `layer/Dockerfile` installs Codex.
_ROOT = "/harness/codex-0.167.0"

# The values of Codex's `model_reasoning_effort`, how hard the model reasons.
_EFFORTS = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]

# The ways Codex can send the model its tools, which the model's upstream must forward.
# `native`: as the model's catalog entry has it, the one `CATALOG` carries if it carries one and
# Codex's own otherwise, in forms only some upstreams forward: an `additional_tools` input item in
# place of `tools`, and `custom` and `namespace` tools. `functions`: each as a `function` tool in
# the request's `tools`, the form every upstream of the Responses API forwards, for an upstream
# that forwards no other, through which Codex would have no tools at all. Codex then offers no
# tool it has only other forms for: not code mode's or sub-agents', and not `apply_patch`, which
# the model runs through the shell instead.
_TOOLS = ["native", "functions"]

# What `functions` sets in the model's catalog entry.
_FUNCTIONS = {
    # An `additional_tools` input item lists the tools in place of `tools`.
    "use_responses_lite": False,
    # Code mode offers one `custom` tool, `exec`, which runs the others.
    "tool_mode": "direct",
    # `apply_patch` has no other form than `freeform`, a `custom` tool; the model runs it through
    # the shell, on whose `PATH` Codex puts it.
    "apply_patch_tool_type": None,
    # Sub-agents' tools are a `namespace` under either multi-agent version.
    "multi_agent_version": "disabled",
    # `clock` is a `namespace`, and `send_user_message_async` adds a second way to ask a user,
    # beside `request_user_input`, which stays.
    "experimental_supported_tools": [],
}

# The request headers Codex sends that the route passes upstream, besides the Responses API's
# own: those that describe its client or its session, and two that name a form of its requests,
# which the route reads in any form: `x-codex-beta-features` the form of compaction they use, and
# `x-openai-internal-codex-responses-lite` the form that lists the tools in an `additional_tools`
# input item rather than in `tools`.
_REQUEST_HEADERS = [
    "originator",
    "user-agent",
    "session-id",
    "thread-id",
    "x-codex-beta-features",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-openai-internal-codex-responses-lite",
]

# The response headers Codex reads that the route passes back, besides the API's own: the model
# that answered, the request's ids, its credits and why a limit was reached, and an error's
# details. Each rate-limit header is named whole, for Codex's one limit, `codex`.
_RESPONSE_HEADERS = [
    "openai-model",
    "x-reasoning-included",
    "x-models-etag",
    "x-codex-turn-state",
    "x-codex-safety-buffering-enabled",
    "x-codex-safety-buffering-faster-model",
    "x-codex-credits-has-credits",
    "x-codex-credits-unlimited",
    "x-codex-credits-balance",
    "x-codex-promo-message",
    "x-codex-rate-limit-reached-type",
    "x-codex-active-limit",
    "x-oai-request-id",
    "cf-ray",
    "x-openai-authorization-error",
    "x-error-json",
] + ["x-codex-" + end for end in [
    "primary-used-percent",
    "primary-window-minutes",
    "primary-reset-at",
    "secondary-used-percent",
    "secondary-window-minutes",
    "secondary-reset-at",
    "limit-name",
]]

# Codex's `config.toml`, with `model`, `catalog` and `url` to fill as TOML, `effort` as the text
# of a TOML string, and `threads` as an int.
_CONFIG = """model = {model}
model_reasoning_effort = "{effort}"
{catalog}model_provider = "pumpkin"
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
base_url = {url}
wire_api = "responses"
requires_openai_auth = false
# A run of hours meets transient failures.
request_max_retries = 100
stream_max_retries = 100

# The lead agent and as many spawned ones at most, each with one model request in flight at a
# time: as many requests as the model route takes from a sandbox at once.
[agents]
max_concurrent_threads_per_session = {threads}

[analytics]
enabled = false

[features]
plugins = false
apps = false
image_generation = false

[plugins._default]
enabled = false
"""

def codex(
        base,
        *,
        model,
        effort,
        workspace,
        prompt,
        deadline_seconds,
        finalize_grace_seconds,
        tools):
    """An agent role's `image`, `runtime` and `egress`, for Codex on `model` at `effort`.

    Codex runs once, as `codex exec`, on the prompt it reads on its standard input, with its files
    readable by the agent's user alone. It takes the arguments every harness takes, in the sense
    `crates/library` gives them, and `tools`, its own.

    Args:
      base: as every harness.
      model: as every harness; Codex's `model`.
      effort: as every harness; Codex's `model_reasoning_effort`: one of `_EFFORTS`.
      workspace: as every harness; it holds Codex's temporary directory too.
      prompt: as every harness; what Codex reads on its standard input, which may be empty.
      deadline_seconds: as every harness.
      finalize_grace_seconds: as every harness.
      tools: how Codex sends the model its tools: one of `_TOOLS`. `functions` needs `CATALOG` to
        carry the model's entry.

    Returns:
      A dict of `image`, `base` with Codex added; `runtime`, the agent's runtime that runs it; and
      `egress`, the route to `model`, on the Responses API, with the headers Codex sends and reads.
    """
    if effort not in _EFFORTS:
        fail("`{}` is not a reasoning effort: use one of {}".format(effort, ", ".join(_EFFORTS)))
    if prompt == None:
        fail("`None` is not a prompt: give a string, which may be empty")
    entries = json.decode(CATALOG)["models"]
    carried = [entry for entry in entries if entry["slug"] == model]
    if tools == "native":
        catalog = carried
    elif tools == "functions":
        if not carried:
            slugs = " or ".join(["`{}`".format(entry["slug"]) for entry in entries])
            message = (
                "Codex's catalog entry for `{}` isn't carried, and sending its tools as " +
                "functions needs it: use {}"
            )
            fail(message.format(model, slugs))
        catalog = [carried[0] | _FUNCTIONS]
    else:
        fail("`{}` is not a way to send Codex's tools: use one of {}".format(tools, ", ".join(_TOOLS)))

    # A normalized absolute path ends in `/` only if it is `/`.
    below = "" if workspace == "/" else workspace

    # `CODEX_HOME`. It can't be in the agent's log directory, where a workflow puts nothing, and
    # where the links Codex keeps in it would fail the collect.
    home = below + "/.codex"

    # `TMPDIR`, beside the home: Codex refuses to make its helpers' links in a home below its
    # temporary directory.
    tmp = below + "/.tmp"
    catalog_path = home + "/model-catalog.json"

    # A JSON string is also a TOML basic string.
    config = _CONFIG.format(
        model = json.encode(model),
        effort = effort,
        catalog = "model_catalog_json = {}\n".format(json.encode(catalog_path)) if catalog else "",
        url = json.encode(MODEL_URL),
        threads = MAX_MODEL_REQUESTS - 1,
    )
    files = [{"path": home + "/config.toml", "mode": "0600", "content": config}]
    if catalog:
        content = json.encode({"models": catalog})
        files = files + [{"path": catalog_path, "mode": "0600", "content": content}]
    return {
        "image": image("Dockerfile", context = "@pumpkin//harness/codex/layer", contexts = {"base": base}),
        "runtime": {"agent": {
            "command": {
                # `exec` runs one turn, unattended, on the prompt it reads on its standard input
                # (`-`), and writes its events to its standard output as JSON lines (`--json`),
                # which the run's record keeps; it writes no session files (`--ephemeral`), reads
                # no command rules (`--ignore-rules`), and works in any directory, a Git
                # repository or not.
                "argv": [
                    _ROOT + "/bin/codex",
                    "exec",
                    "--ephemeral",
                    "--ignore-rules",
                    "--skip-git-repo-check",
                    "--json",
                    "-",
                ],
                "env": ["CODEX_HOME=" + home, "TMPDIR=" + tmp],
                "cwd": workspace,
            },
            "dirs": [{"path": home, "mode": "0700"}, {"path": tmp, "mode": "0700"}],
            "files": files,
            "stdin": prompt,
            "deadline_seconds": deadline_seconds,
            "finalize_grace_seconds": finalize_grace_seconds,
        }},
        "egress": {"model": [{
            "api": "responses",
            "models": [model],
            "request_headers": _REQUEST_HEADERS,
            "response_headers": _RESPONSE_HEADERS,
        }]},
    }
