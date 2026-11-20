"""Two scripted agents test and submit C++ programs for hypergraph core numbers."""

load("//team/workflow.star", "PARTICIPANTS", "team")

OMIT = arg("omit", str, default = "none")
OMIT_MODE = OMIT if OMIT in ["none", "bob", "all"] else fail("omit must be none, bob or all")

toolchain = image("runner/Dockerfile")
team(
    "examples/team",
    image("Dockerfile", target = "reference", contexts = {"toolchain": toolchain}),
    {role: {
        "command": {"argv": ["python3", "/opt/team/agent.py", role, OMIT_MODE], "cwd": "/pumpkin/home"},
        "finalize_grace_seconds": 2,
    } for role in PARTICIPANTS},
    toolchain = toolchain,
    needs = {"cpu_millis": 500, "memory_bytes": 256 * MiB},
    deadline_seconds = 90,
)
