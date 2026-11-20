"""Native benchmark images and their content pins."""

load("//benchmark-input/images.star", "AGENT_REPOSITORY", "VERIFIER_REPOSITORY")

def agent_image():
    """Builds the agent image with its native packager.

    Returns:
      The native C++ development environment for Codex.
    """
    return image("Dockerfile", context = "//hypergraph-benchmark/agent", mount = {
        "binding": "//benchmark-input/agent",
    }, args = {
        "BASE": AGENT_REPOSITORY + "@sha256:4af4092b321758c658b5058da3a3d3bc277ac3547188876b1dacdde86b770bba",
    })

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
    return image("Dockerfile", context = "//hypergraph-benchmark/judge", target = stage, mount = mounts, args = {
        "BASE": VERIFIER_REPOSITORY + "@sha256:928a04bc4b49b1e18af8bd716a6b11157bbaba856901bbc88628d9adaf774874",
    })
