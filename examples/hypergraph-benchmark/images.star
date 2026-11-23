"""Images built from the benchmark's starter and Ligra's source, which a pinned commit fetches."""

_DIR = "//hypergraph-benchmark/"

def _contexts():
    ligra = image("Dockerfile", context = _DIR + "ligra")
    return {"ligra": ligra, "base": image("Dockerfile", context = _DIR + "base", contexts = {"ligra": ligra})}

def agent_image():
    """Builds the agent image with its native packager.

    Returns:
      The native C++ development environment for Codex.
    """
    return image("Dockerfile", context = _DIR + "agent", contexts = {"base": _contexts()["base"]})

def judge_image(stage):
    """Builds the development, final, or replay verifier.

    Args:
      stage: train, eval, or replay.

    Returns:
      The verifier with the data for that stage.
    """
    mounts = {"selections": "//benchmark-replay"} if stage == "replay" else {}
    return image("Dockerfile", context = _DIR + "judge", target = stage, mount = mounts, contexts = _contexts())
