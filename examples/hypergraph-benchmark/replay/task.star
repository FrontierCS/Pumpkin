"""Re-evaluate the exact selected archives in one independent remote verifier."""

load("//hypergraph-benchmark/images.star", "judge_image")
load("//hypergraph-benchmark/replay/calls.star", "replay", "start")

workflow(
    name = "examples/hypergraph-benchmark/replay",
    deadline_seconds = 60,
    roles = [
        {
            "name": "agent",
            "image": image("agent.Dockerfile"),
            "user": {"uid": 1000, "gid": 1000},
            "needs": {"cpu_millis": 500, "memory_bytes": 128 * MiB},
            "runtime": {"agent": {
                "command": {"argv": ["sh", "-ec", "echo ready > ready && exec /.pumpkin/bin/sandboxctl call start ready"], "cwd": "/pumpkin/home"},
                "finalize_grace_seconds": 1,
                "grants": [{"name": "start", "now": {"body": start, "payload_max_bytes": 16}}],
            }},
        },
        {
            "name": "judge",
            "image": judge_image("replay"),
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 8000, "memory_bytes": 4 * GiB},
            "runtime": {"target": {"cwd": "/pumpkin/home"}},
        },
    ],
    final = {"target": "judge", "body": replay, "payload_max_bytes": 16, "timeout_seconds": 9200},
)
