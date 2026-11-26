"""A check of a runner's companions, with no model, for a provider whose classes offer them.

The final call leases a runner whose sandbox holds `companions` companions beside `main`, at
least 1, on the needs that `cpu_millis` and `memory_mib` give. The defaults give 8 companions far
more than the least needs that hold them. `docs/providers.md` says why for each provider that
offers companions.

Each container serves its own name on the sandbox's network, and says the user that its command
runs as. Each companion fetches the names of `main` and of the companion before it, and `main`
fetches each companion's. The first companion tries a public address. `main` times its round
trips with the first companion and with itself. The score counts the facts that hold.
"""

load("//companions/final.star", "final_call")

COMPANIONS = arg("companions", int, default = 8)
CPU_MILLIS = arg("cpu_millis", int, default = 2000)
MEMORY_MIB = arg("memory_mib", int, default = 2048)

# How long the final call holds its lease, idle, once its probes are done, as a measure of what an
# idle sandbox of companions costs the driver. A command in each companion follows.
HOLD_SECONDS = arg("hold_seconds", int, default = 0)

IMAGE = image("Dockerfile")
USER = {"uid": 1000, "gid": 1000}

# Each companion, in order, with a user of its own, and the name of the one before it. A list
# keeps its order in the final call's keywords, where a dict's keys are sorted.
DIGITS = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
INDICES = [100 * hundreds + 10 * tens + units for hundreds in DIGITS for tens in DIGITS for units in DIGITS]
DECLARED = [
    {"name": "c{}".format(index), "user": {"uid": 2000 + index, "gid": 2000 + index}} | ({"before": "c{}".format(index - 1)} if index > 0 else {})
    for index in INDICES
    if index < COMPANIONS
]

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
            "needs": {"cpu_millis": CPU_MILLIS, "memory_bytes": MEMORY_MIB * MiB},
            "runtime": {"runner": {
                "cwd": "/workspace",
                "companions": [
                    {"name": companion["name"], "image": IMAGE, "user": companion["user"], "cwd": "/workspace"}
                    for companion in DECLARED
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
                "session": {"runner": "runner", "instances": 1, "instance_seconds": 600 + HOLD_SECONDS, "commands": 2 * COMPANIONS + 5, "transfer_bytes": 1 * KiB},
            }]}},
        },
    ],
    final = {
        "target": "judge",
        "body": final_call,
        "keywords": {"user": USER, "companions": DECLARED, "hold_seconds": HOLD_SECONDS},
        "payload_max_bytes": 1 * KiB,
        "timeout_seconds": 900 + HOLD_SECONDS,
    },
)
