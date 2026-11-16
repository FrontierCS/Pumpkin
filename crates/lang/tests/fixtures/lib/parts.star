"""The parts of a neutral workflow that the fixtures' workflows share: `AGENT`, `JUDGE` and
`SCORE` make a whole one, which a case changes a field of, as `JUDGE | {"status": "ready"}`."""

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
        "grants": [{"name": "final", "submit": {}}],
    }},
}

JUDGE_IMAGE = "sha256:2222222222222222222222222222222222222222222222222222222222222222"

JUDGE_USER = {"uid": 65532, "gid": 65532}

# What every judge returns of its result.
RESULT = {"shows": {"score": "/score"}, "returns": ["/score", "/failure"]}

JUDGE_RUNTIME = {"judge": {
    "command": {"argv": ["/opt/judge/run"], "cwd": "/pumpkin/home"},
    "slots": [{"name": "payload", "max_bytes": 64 * KiB}],
    "result": RESULT,
}}

# The judge that scores, as `SCORE` names it.
JUDGE = {
    "name": "final_judge",
    "image": JUDGE_IMAGE,
    "user": JUDGE_USER,
    "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
    "runtime": JUDGE_RUNTIME,
}

SCORE = {"role": "final_judge", "slot": "payload", "timeout_seconds": 120}

_DRAFT = "private to this file"
