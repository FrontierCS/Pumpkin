"""Images built from the pinned native benchmark source."""

def _native(role):
    generated = image("Dockerfile", context = "//hypergraph-benchmark/generation", mount = {
        "source": "//benchmark-input/generation",
    })
    return image("Dockerfile", context = "//benchmark-input/native-" + role, contexts = {
        "generated": generated,
        "shared": image("Dockerfile", context = "//benchmark-input/" + role + "-base"),
    })

def agent_image():
    """Builds the agent image with its native packager.

    Returns:
      The native C++ development environment for Codex.
    """
    return image("Dockerfile", context = "//hypergraph-benchmark/agent", mount = {
        "binding": "//benchmark-input/agent",
    }, contexts = {"native": _native("agent")})

def judge_image(stage):
    """Builds the development, final, or replay verifier.

    Args:
      stage: train, eval, or replay.

    Returns:
      The verifier with the data for that stage.
    """
    mounts = {"binding": "//benchmark-input/judge"}
    if stage == "replay":
        mounts = mounts | {"selections": "//benchmark-replay"}
    return image("Dockerfile", context = "//hypergraph-benchmark/judge", target = stage, mount = mounts, contexts = {
        "native": _native("verifier"),
    })
