"""A workflow that needs no model: the agent counts its greeting's lines and bytes by a call of its
job function `count`, then submits the greeting, and the judge scores its size and counts its lines."""

load("//hello/count.star", "count")

IMAGE = image("Dockerfile")
SANDBOXCTL = "/.pumpkin/bin/sandboxctl"

# Calls `count`, prints the job's status once it has ended, and submits the greeting.
AGENT = """
echo hello > greeting
SANDBOXCTL call count greeting
while SANDBOXCTL status 0 | grep -q -E '"state":"(queued|running)"'; do sleep 1; done
SANDBOXCTL status 0
SANDBOXCTL submit final greeting --finalize
""".replace("SANDBOXCTL", SANDBOXCTL)

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
                    "argv": ["sh", "-c", "set -e" + AGENT],
                    "cwd": "/workspace",
                },
                "deadline_seconds": 300,
                "finalize_grace_seconds": 30,
                "grants": [
                    {"name": "count", "job": {
                        "target": "counter",
                        "body": count,
                        "payload_max_bytes": 1 * KiB,
                        "call_timeout_seconds": 60,
                    }},
                    {"name": "final", "submit": {}},
                ],
            }},
        },
        {
            "name": "counter",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
            "runtime": {"target": {"cwd": "/workspace"}},
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
