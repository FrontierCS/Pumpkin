"""A workflow that needs no model: the agent submits a greeting, and the judge scores its size and counts its lines."""

IMAGE = image("Dockerfile")
SANDBOXCTL = "/.pumpkin/bin/sandboxctl"

workflow(
    name = "examples/hello",
    roles = [
        {
            "name": "agent",
            "image": IMAGE,
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"agent": {
                "command": {
                    "argv": ["sh", "-c", "echo hello > greeting && " + SANDBOXCTL + " submit final greeting --finalize"],
                    "cwd": "/workspace",
                },
                "deadline_seconds": 300,
                "finalize_grace_seconds": 30,
                "grants": [{"submit": {"name": "final"}}],
            }},
        },
        {
            "name": "judge",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"judge": {
                "command": {
                    "argv": ["sh", "-c", "printf '{\"score\": %s, \"lines\": %s}' $(wc -c < /pumpkin/in/payload/raw) $(wc -l < /pumpkin/in/payload/raw) > /pumpkin/out/result.json"],
                    "cwd": "/pumpkin/home",
                },
                "slots": [{"name": "payload", "max_bytes": 1 * KiB}],
                "result": {"shows": {"score": "/score", "lines": "/lines"}, "returns": ["/score"]},
            }},
        },
    ],
    score = {"role": "judge", "slot": "payload", "timeout_seconds": 60},
)
