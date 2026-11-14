"""A workflow whose agent is Codex, on the model that `--arg model=<name>` names: it writes a
greeting and submits it, and the judge scores the greeting's size."""

MODEL = arg("model", str)
EFFORT = arg("effort", str, default = "high")

IMAGE = image("Dockerfile")

agent = codex(
    IMAGE,
    model = MODEL,
    effort = EFFORT,
    tools = "native",
    workspace = "/workspace",
    prompt = "Write a greeting to the file `greeting`, then submit it with `sandboxctl submit final greeting --finalize`.\n",
    deadline_seconds = 1800,
    finalize_grace_seconds = 60,
)

workflow(
    name = "examples/codex",
    roles = [
        {
            "name": "agent",
            "image": agent["image"],
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
            "runtime": agent["runtime"],
            "egress": agent["egress"],
            "grants": [{"submit": {"name": "final"}}],
        },
        {
            "name": "judge",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"judge": {
                "command": {
                    "argv": ["sh", "-c", "printf '{\"score\": %s}' $(wc -c < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"],
                    "cwd": "/pumpkin/home",
                },
                "slots": [{"name": "payload", "max_bytes": 1 * KiB}],
                "result": {"score": "/score", "returns": ["/score"]},
            }},
        },
    ],
    score = {"role": "judge", "slot": "payload", "timeout_seconds": 60},
)
