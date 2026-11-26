"""A check of a role's assets, with no model, for a provider whose classes hold assets.

The runner holds an asset at `/opt/model`: two license files of ripgrep on GitHub, at a pinned
revision. The final call leases the runner, checks each file's SHA-256, and tries each write that
a read-only mount refuses, as the runner's user. A second lease, in a fresh sandbox, checks the
SHA-256s again. The score counts the facts that hold.
"""

load("//assets/final.star", "final_call")

IMAGE = image("Dockerfile")
USER = {"uid": 1000, "gid": 1000}

REVISION = "https://raw.githubusercontent.com/BurntSushi/ripgrep/e50df40a1967708b9781486b1c017e48040bceb0/"

FILES = [
    {
        "path": "LICENSE-MIT",
        "url": REVISION + "LICENSE-MIT",
        "size": 1081,
        "sha256": "sha256:0f96a83840e146e43c0ec96a22ec1f392e0680e6c1226e6f3ba87e0740af850f",
    },
    {
        "path": "UNLICENSE",
        "url": REVISION + "UNLICENSE",
        "size": 1211,
        "sha256": "sha256:7e12e5df4bae12cb21581ba157ced20e1986a0508dd10d0e8a4ab9a4cf94e85c",
    },
]

PATH = "/opt/model"

workflow(
    deadline_seconds = 60,
    name = "examples/assets",
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
            "assets": [{"path": PATH, "files": FILES}],
            "user": USER,
            "needs": {"cpu_millis": 250, "memory_bytes": 256 * MiB},
            "runtime": {"runner": {"cwd": "/workspace"}},
        },
        {
            "name": "judge",
            "image": IMAGE,
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 250, "memory_bytes": 256 * MiB},
            "runtime": {"target": {"cwd": "/pumpkin/home", "sessions": [{
                "name": "sandbox",
                "session": {"runner": "runner", "instances": 1, "instance_seconds": 120, "commands": 3, "transfer_bytes": 1 * KiB},
            }]}},
        },
    ],
    final = {
        "target": "judge",
        "body": final_call,
        "keywords": {"path": PATH, "files": FILES},
        "payload_max_bytes": 1 * KiB,
        "timeout_seconds": 300,
    },
)
