"""The scripted and Codex agents for the hypergraph task."""

load("@pumpkin//harness/codex/codex.star", "codex")

_PROMPT = """Compute the core number of every vertex in /opt/hypergraph/graph.json.
The vertices are numbered from zero. A hyperedge contributes one to each member's degree.
When a vertex is removed, every edge containing it disappears. Repeated edges count separately.
A vertex's core number is the largest k for which it belongs to a set of vertices that all have
degree at least k using only edges wholly in the set. An isolated vertex has core number zero.

Write cores.json as a JSON array of integers in vertex order. To score a draft, run
/.pumpkin/bin/sandboxctl call eval cores.json and poll /.pumpkin/bin/sandboxctl status JOB
with the returned job number. A score of 1 means every entry matches. Submit the final file with
/.pumpkin/bin/sandboxctl call final cores.json --arg finalize=true.
"""

def agent(model: str):
    """Builds the agent that computes and submits the vector.

    Args:
      model: the Codex model, or an empty string to run the reference agent.

    Returns:
      The agent's image and runtime, to which the task adds its grants.
    """
    if not model:
        return image("Dockerfile", target = "reference"), {
            "command": {"argv": ["python3", "/opt/hypergraph/reference.py"], "cwd": "/workspace"},
            "deadline_seconds": 300,
            "finalize_grace_seconds": 0,
        }
    harness = codex(
        image("Dockerfile", target = "graph"),
        model = model,
        effort = "low",
        tools = "native",
        workspace = "/workspace",
        prompt = _PROMPT,
        deadline_seconds = 300,
        finalize_grace_seconds = 0,
    )
    runtime = harness["runtime"]["agent"]
    routes = [route | {"budget": {"microusd": USD // 2}} for route in runtime["egress"]["model"]]
    return harness["image"], runtime | {"egress": {"model": routes}}
