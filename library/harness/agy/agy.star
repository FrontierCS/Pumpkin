"""agy, Google's Antigravity CLI, as an agent's harness: `agy` adds agy 1.5.16 to an agent's image
and writes the runtime of one unattended `antigravity --print`, with the route to its model.

agy works in its workspace, and keeps its home (`HOME`), with its settings, its conversations and
its logs, and its temporary files (`TMPDIR`) in a directory of its own there. It writes each event
of its turn to its standard output as a line of JSON. Its model requests go only to the endpoint
`sandboxd` serves, `MODEL_URL`, on the Gemini API, with a stand-in key: the host holds the key,
and the route passes no client's.

The route allows the agent's model alone: it refuses a request agy makes on any other.
"""

# Where `layer/Dockerfile` installs agy.
_ROOT = "/harness/agy-1.5.16"

# The models agy 1.5.16 starts on that the Gemini API serves under the name agy asks it for, each
# with the thinking levels agy runs it at.
_EFFORTS = {
    "gemini-4-hydrogen": ["low", "medium", "high", "xhigh"],
    "gemini-4-neon": ["low", "medium", "high", "xhigh"],
    "gemini-4-carbon": ["low", "medium", "high"],
    "gemini-4-argon": ["low", "medium", "high"],
}

# Gemini's API as the provider, whose key the environment gives. Nothing sent but to the model: no
# telemetry and no notification. No question asked: every tool call and artifact goes ahead, in
# the workspace or outside it, without agy's own sandbox, for the agent's sandbox confines it.
_SETTINGS = {
    "modelProvider": "gemini",
    "enableTelemetry": False,
    "notifications": False,
    "toolPermission": "always-proceed",
    "artifactReviewPolicy": "always-proceed",
    "allowNonWorkspaceAccess": True,
    "enableTerminalSandbox": False,
}

def agy(
        base,
        *,
        model,
        effort,
        workspace,
        prompt,
        finalize_grace_seconds):
    """An agent role's `image` and `runtime`, for agy on `model` at `effort`.

    agy runs once, as `antigravity --print`, unattended, with its files readable by the agent's
    user alone. It takes the arguments every harness takes, in the sense `crates/library` gives
    them, and none of its own.

    Args:
      base: as every harness, and must have `bash`, which agy's shell tool runs.
      model: as every harness; the model agy is started on: one `_EFFORTS` names.
      effort: as every harness; the thinking level agy is started at: one of `_EFFORTS[model]`.
      workspace: as every harness.
      prompt: as every harness; not empty.
      finalize_grace_seconds: as every harness.

    Returns:
      A dict of `image`, `base` with agy added; and `runtime`, the agent's runtime that runs it,
      with its `egress`, the route to `model`, on the Gemini API.
    """
    if model not in _EFFORTS:
        fail("`{}` is not a model that agy starts on: use one of {}".format(model, ", ".join(_EFFORTS.keys())))
    if effort not in _EFFORTS[model]:
        fail("`{}` is not a thinking level agy runs `{}` at: use one of {}".format(effort, model, ", ".join(_EFFORTS[model])))
    if prompt == None or prompt == "":
        fail("agy runs only on a prompt: give a string that is not empty")

    # A normalized absolute path ends in `/` only if it is `/`.
    home = ("" if workspace == "/" else workspace) + "/.agy-home"
    tmp = home + "/tmp"
    settings = home + "/.gemini/antigravity-cli"
    return {
        "image": image("Dockerfile", context = "@pumpkin//harness/agy/layer", contexts = {"base": base}),
        "runtime": {"agent": {
            "command": {
                # `antigravity`, started directly, which `layer/Dockerfile` makes run on any
                # base. `--print` takes the prompt whole, whatever it begins with, given as
                # `--print=<text>`. agy names its model and its thinking level as one.
                "argv": [
                    _ROOT + "/antigravity",
                    "--print=" + prompt,
                    "--model",
                    model + "-" + effort,
                    "--output-format",
                    "stream-json",
                    "--dangerously-skip-permissions",
                    "--disable-slash-commands",
                    "--log-file",
                    home + "/agy.log",
                ],
                "env": [
                    "HOME=" + home,
                    "TMPDIR=" + tmp,
                    # agy sends its requests to `<base URL>/v1beta/models/<model>:<method>`.
                    "GOOGLE_GEMINI_BASE_URL=" + MODEL_URL,
                    # agy starts only with a key, which it sends as `x-goog-api-key`, and the
                    # route replaces with the deployment's.
                    "GEMINI_API_KEY=pumpkin",
                    # No update: the model route is the only way out of the sandbox.
                    "AGY_CLI_DISABLE_AUTO_UPDATE=1",
                ],
                "cwd": workspace,
            },
            "dirs": [{"path": path, "mode": "0700"} for path in [home, tmp, home + "/.gemini", settings]],
            "files": [
                {"path": settings + "/settings.json", "mode": "0600", "content": json.encode(_SETTINGS)},
            ],
            "finalize_grace_seconds": finalize_grace_seconds,
            "egress": {"model": [{"api": "generate_content", "models": [model]}]},
        }},
    }
