"""Find the core numbers of a hypergraph, with Codex on a named model."""

load("@pumpkin//harness/codex/codex.star", "codex")
load("//hypergraph/score.star", "final_score", "score")
load("//submit.star", "submission")

MODEL = arg("model", str)
PAYLOAD_BYTES = 1 * KiB

PROMPT = """Compute the core number of every vertex in /opt/hypergraph/graph.json.
The vertices are numbered from zero. A hyperedge contributes one to each member's degree.
When a vertex is removed, every edge containing it disappears. Repeated edges count separately.
A vertex's core number is the largest k for which it belongs to a set of vertices that all have
degree at least k using only edges wholly in the set. An isolated vertex has core number zero.

Write cores.json as a JSON array of integers in vertex order. Score your draft, correct any
error you find and finalize your best vector. A score of 1 means every entry matches.
"""

agent = codex(
    image("Dockerfile", target = "graph"),
    model = MODEL,
    effort = "low",
    tools = "native",
    workspace = "/workspace",
    prompt = PROMPT,
    finalize_grace_seconds = 0,
)
runtime = agent["runtime"]["agent"]
routes = [route | {"budget": {"microusd": USD // 2}} for route in runtime["egress"]["model"]]
judge_image = image("Dockerfile", target = "judge")

workflow(
    deadline_seconds = 300,
    name = "examples/hypergraph",
    roles = [{
        "name": "agent",
        "image": agent["image"],
        "user": {"uid": 1000, "gid": 1000},
        "needs": {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
        "runtime": {"agent": runtime | {"egress": {"model": routes}, "grants": [
            {"name": "eval", "help": "Score a draft JSON core vector. Returns its score.", "job": {
                "target": "evaluator",
                "body": score,
                "payload_max_bytes": PAYLOAD_BYTES,
                "calls": 2,
                "call_timeout_seconds": 30,
            }},
            submission("final", PAYLOAD_BYTES),
        ]}},
    }] + [{
        "name": name,
        "image": judge_image,
        "user": {"uid": 65532, "gid": 65532},
        "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
        "runtime": {"target": {"cwd": "/pumpkin/home"}},
    } for name in ["evaluator", "judge"]],
    final = {"target": "judge", "body": final_score, "payload_max_bytes": PAYLOAD_BYTES, "timeout_seconds": 30},
)
