"""The parts of a neutral workflow that the fixtures' workflows share: `AGENT`, `FINAL_TARGET` and
`FINAL` make a whole one, which a case changes a field of, as `FINAL_TARGET | {"status": "ready"}`."""

load("//lib/submit.star", "submit")

AGENT = {
    "name": "agent",
    "image": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
    "user": {"uid": 1000, "gid": 1000},
    "needs": {"cpu_millis": 2000, "memory_bytes": 4 * GiB},
    "runtime": {"agent": {
        "command": {"argv": ["/usr/local/bin/agent"], "cwd": "/workspace"},
        "stdin": "Solve the task in /task/README.md.\n",
        "deadline_seconds": 3600,
        "finalize_grace_seconds": 0,
        "grants": [{"name": "final", "now": {"body": submit, "payload_max_bytes": 64 * KiB}}],
    }},
}

JUDGE_IMAGE = "sha256:2222222222222222222222222222222222222222222222222222222222222222"

JUDGE_USER = {"uid": 65532, "gid": 65532}

# The final call's target, as `FINAL` names it.
FINAL_TARGET = {
    "name": "final_judge",
    "image": JUDGE_IMAGE,
    "user": JUDGE_USER,
    "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
    "runtime": {"target": {"cwd": "/pumpkin/home"}},
}

def final_call(target: Target, payload: Payload):
    """Runs the judge on the selection, and shows the result it wrote."""
    target.put("/pumpkin/in/payload", payload)
    target.run(["/opt/judge/run"])
    return {"result": target.get("/pumpkin/out/result.json")}

FINAL = {"target": "final_judge", "body": final_call, "payload_max_bytes": 64 * KiB, "timeout_seconds": 120}

_DRAFT = "private to this file"
