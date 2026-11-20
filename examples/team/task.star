"""Two agents predict (row · [2, 3] + error) modulo 13 for two public rows.

Alice predicts amber, whose row is [3, 4] and whose error is -1. Bob checks her message on the
board and adds blue, whose row is [2, 5] and whose error is 1. Each correct prediction is worth
one point. The judge reports each role's score and takes the highest. Pass `--arg omit=bob`
to see one missing selection, or `--arg omit=all` to see both missing.
"""

load("//submit.star", "submit")
load("//team/bodies.star", "board_call", "judge")

OMIT = arg("omit", str, default = "none")
OMIT_MODE = OMIT if OMIT in ["none", "bob", "all"] else fail("omit must be none, bob or all")

IMAGE = image("Dockerfile")
PARTICIPANTS = ["alice", "bob"]
USER = {"uid": 1000, "gid": 1000}
NEEDS = {"cpu_millis": 500, "memory_bytes": 256 * MiB}
GRANT = {"name": "board", "job": {
    "target": "board",
    "body": board_call,
    "payload_max_bytes": 16 * KiB,
    "call_timeout_seconds": 10,
    "calls": 200,
}}

workflow(
    name = "examples/team",
    deadline_seconds = 90,
    roles = [
        {
            "name": role,
            "image": IMAGE,
            "user": USER,
            "needs": NEEDS,
            "runtime": {"agent": {
                "command": {"argv": ["python3", "/opt/team/agent.py", role, OMIT_MODE], "cwd": "/pumpkin/home"},
                "finalize_grace_seconds": 2,
                "grants": [GRANT, {"name": "select_" + role, "now": {"body": submit, "payload_max_bytes": 1 * KiB}}],
            }},
        }
        for role in PARTICIPANTS
    ] + [
        {
            "name": "board",
            "image": image("board/Dockerfile"),
            "user": USER,
            "needs": NEEDS,
            "runtime": {"shared": {
                "command": {"argv": ["python3", "/opt/team/board.py", "serve"] + PARTICIPANTS, "cwd": "/pumpkin/home"},
                "cwd": "/pumpkin/home",
            }},
        },
        {
            "name": "judge",
            "image": image("judge/Dockerfile"),
            "user": USER,
            "needs": NEEDS,
            "runtime": {"target": {"cwd": "/pumpkin/home"}},
        },
    ],
    final = {"target": "judge", "body": judge, "input": "selections", "payload_max_bytes": 1 * KiB, "timeout_seconds": 30},
)
