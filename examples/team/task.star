"""Two agents predict (row · [2, 3] + error) modulo 13 for two public rows.

Alice predicts amber, whose row is [3, 4] and whose error is -1. Bob checks her message on the
board and adds blue, whose row is [2, 5] and whose error is 1. Each correct prediction is worth
one point. The judge reports each role's score and takes the highest. Pass `--arg omit=bob`
to see one missing selection, or `--arg omit=all` to see both missing.
"""

load("//team/workflow.star", "PARTICIPANTS", "team")

OMIT = arg("omit", str, default = "none")
OMIT_MODE = OMIT if OMIT in ["none", "bob", "all"] else fail("omit must be none, bob or all")

team(
    "examples/team",
    image("Dockerfile", target = "reference"),
    {role: {
        "command": {"argv": ["python3", "/opt/team/agent.py", role, OMIT_MODE], "cwd": "/pumpkin/home"},
        "finalize_grace_seconds": 2,
    } for role in PARTICIPANTS},
    needs = {"cpu_millis": 500, "memory_bytes": 256 * MiB},
    deadline_seconds = 90,
)
