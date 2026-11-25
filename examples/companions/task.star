"""A check of a runner's companions, with no model, for a provider whose classes offer them.

The final call leases a runner whose sandbox holds `companions` companions beside `main`. The
runner's needs hold exactly 8 on Modal. Each container serves its own name on the sandbox's
network, and says the user that its command runs as. Each companion fetches `main`'s name, and
`main` fetches each companion's. The score counts the commands that said what was expected.
"""

load("//companions/final.star", "final_call")

COMPANIONS = arg("companions", int, default = 2)

IMAGE = image("Dockerfile")
USER = {"uid": 1000, "gid": 1000}

# Each companion's user, by the companion's name: a user of its own.
DIGITS = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
INDICES = [100 * hundreds + 10 * tens + units for hundreds in DIGITS for tens in DIGITS for units in DIGITS]
USERS = {"c{}".format(index): {"uid": 2000 + index, "gid": 2000 + index} for index in INDICES if index < COMPANIONS}

workflow(
    deadline_seconds = 60,
    name = "examples/companions",
    roles = [
        {
            "name": "agent",
            "image": IMAGE,
            "user": USER,
            "needs": {"cpu_millis": 250, "memory_bytes": 256 * MiB},
            "runtime": {"agent": {
                "command": {"argv": ["true"], "cwd": "/workspace"},
                "finalize_grace_seconds": 0,
                "grants": [],
            }},
        },
        {
            "name": "runner",
            "image": IMAGE,
            "user": USER,
            "needs": {"cpu_millis": 256, "memory_bytes": 256 * MiB},
            "runtime": {"runner": {
                "cwd": "/workspace",
                "companions": [
                    {"name": name, "image": IMAGE, "user": user, "cwd": "/workspace"}
                    for name, user in USERS.items()
                ],
            }},
        },
        {
            "name": "judge",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 250, "memory_bytes": 256 * MiB},
            "runtime": {"target": {"cwd": "/pumpkin/home", "sessions": [{
                "name": "sandbox",
                "session": {"runner": "runner", "instances": 1, "instance_seconds": 300, "commands": COMPANIONS + 2, "transfer_bytes": 1 * KiB},
            }]}},
        },
    ],
    final = {
        "target": "judge",
        "body": final_call,
        "keywords": {"user": USER, "companions": USERS},
        "payload_max_bytes": 1 * KiB,
        "timeout_seconds": 600,
    },
)
