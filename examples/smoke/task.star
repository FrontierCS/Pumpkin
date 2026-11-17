"""A check of a deployment, with no model, which `cargo xtask live` runs through the controller.

The agent leases a CPU runner, and a GPU runner if `gpu_model` names a GPU's model, runs
`probe.py` in its own sandbox and in each runner, and submits what each printed; the judge runs it
in its own sandbox too, and scores each of its lines that says the sandbox is confined as it should
be: 5 for each sandbox, so 15 without a GPU runner, and 20 with one.
"""

load("//submit.star", "submit")

DEADLINE = arg("deadline_seconds", int, default = 600)
GPU_MODEL = arg("gpu_model", str, default = "")

# With `hold`, the agent opens a lease of each session and holds them, and probes nothing, until its
# deadline: a run that `cargo xtask live --lost` loses once they are ready.
HOLD = arg("hold", bool, default = False)

IMAGE = image("Dockerfile")
JUDGE_IMAGE = image("judge/Dockerfile")
USER = {"uid": 1000, "gid": 1000}

CPU_SESSION = {"name": "cpu", "session": {"runner": "cpu_runner", "instances": 1, "instance_seconds": 300, "commands": 4, "transfer_bytes": 1 * KiB}}
GPU_SESSION = {"name": "gpu", "session": {"runner": "gpu_runner", "instances": 1, "instance_seconds": 300, "commands": 4, "transfer_bytes": 1 * KiB}}
RUNNER = {"runner": {"cwd": "/workspace"}}
GPU = {"count": 1, "model": GPU_MODEL}
GPU_RUNNER = {
    "name": "gpu_runner",
    "image": IMAGE,
    "user": USER,
    "needs": {"cpu_millis": 2000, "memory_bytes": 4 * GiB, "gpu": GPU},
    "runtime": RUNNER,
}

workflow(
    name = "examples/smoke",
    roles = [
        {
            "name": "agent",
            "image": IMAGE,
            "user": USER,
            "needs": {"cpu_millis": 500, "memory_bytes": 512 * MiB},
            "runtime": {"agent": {
                "command": {"argv": ["python3", "-u", "/opt/smoke/agent.py"] + (["hold"] if HOLD else []), "cwd": "/workspace"},
                "deadline_seconds": DEADLINE,
                "finalize_grace_seconds": 0,
                "grants": [CPU_SESSION] + ([GPU_SESSION] if GPU_MODEL else []) + [{"name": "final", "now": {"body": submit, "payload_max_bytes": 64 * KiB}}],
            }},
        },
        {
            "name": "cpu_runner",
            "image": IMAGE,
            "user": USER,
            "needs": {"cpu_millis": 1000, "memory_bytes": 1 * GiB},
            "runtime": RUNNER,
        },
    ] + ([GPU_RUNNER] if GPU_MODEL else []) + [
        {
            "name": "judge",
            "image": JUDGE_IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 500, "memory_bytes": 512 * MiB},
            "runtime": {"judge": {
                "command": {"argv": ["python3", "-u", "/opt/smoke/judge.py"], "cwd": "/pumpkin/home"},
                "slots": [{"name": "payload", "max_bytes": 64 * KiB}],
                "result": {"shows": {"score": "/score"}, "returns": ["/score", "/detail"]},
            }},
        },
    ],
    score = {"role": "judge", "slot": "payload", "timeout_seconds": 300},
)
