"""Find the core numbers of a hypergraph, with a scripted agent or Codex on a named model."""

load("//hypergraph/agent.star", "agent")
load("//hypergraph/score.star", "score")
load("//submit.star", "submit")

MODEL = arg("model", str, default = "")
agent_image, runtime = agent(MODEL)
judge_image = image("Dockerfile", target = "judge")
PAYLOAD_BYTES = 1 * KiB

workflow(
    name = "examples/hypergraph",
    roles = [{
        "name": "agent",
        "image": agent_image,
        "user": {"uid": 1000, "gid": 1000},
        "needs": {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
        "runtime": {"agent": runtime | {"grants": [
            {"name": "eval", "job": {
                "target": "evaluator",
                "body": score,
                "payload_max_bytes": PAYLOAD_BYTES,
                "calls": 2,
                "call_timeout_seconds": 30,
            }},
            {"name": "final", "now": {"body": submit, "payload_max_bytes": PAYLOAD_BYTES}},
        ]}},
    }] + [{
        "name": name,
        "image": judge_image,
        "user": {"uid": 65532, "gid": 65532},
        "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
        "runtime": {"target": {"cwd": "/pumpkin/home"}},
    } for name in ["evaluator", "judge"]],
    final = {"target": "judge", "body": score, "payload_max_bytes": PAYLOAD_BYTES, "timeout_seconds": 30},
)
