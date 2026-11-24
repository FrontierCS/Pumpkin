"""A workflow whose agent is Codex, on the model that `--arg model=<name>` names: it writes a
greeting and submits it, and the final call scores the greeting's size."""

load("@pumpkin//harness/codex/codex.star", "codex")
load("//budget.star", "budgeted")
load("//codex/final.star", "final_call")
load("//submit.star", "submission")

MODEL = arg("model", str)
EFFORT = arg("effort", str, default = "high")

IMAGE = image("Dockerfile")

agent = codex(
    IMAGE,
    model = MODEL,
    effort = EFFORT,
    tools = "native",
    workspace = "/workspace",
    prompt = "Write a greeting to the file `greeting` and finalize it as your submission.\n",
    finalize_grace_seconds = 60,
)

workflow(
    deadline_seconds = 1800,
    name = "examples/codex",
    roles = [
        {
            "name": "agent",
            "image": agent["image"],
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
            "runtime": {"agent": agent["runtime"]["agent"] | {
                "egress": {"model": budgeted(agent["runtime"]["agent"]["egress"]["model"])},
                "grants": [submission("final", 1 * KiB)],
            }},
        },
        {
            "name": "judge",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"target": {"cwd": "/pumpkin/home"}},
        },
    ],
    final = {"target": "judge", "body": final_call, "payload_max_bytes": 1 * KiB, "timeout_seconds": 60},
)
