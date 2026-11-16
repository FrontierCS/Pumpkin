"""A neutral workflow written as literals: an agent, a judge it calls while it works, and the
judge that scores what it submits."""

# The agent's user and group, unpacked from one tuple, on two lines joined.
UID, GID = 1000, \
    1000

workflow(
    name = "neutral/literal",
    roles = [
        {
            "name": "agent",
            "image": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "user": {"uid": UID, "gid": GID},
            "needs": {
                "cpu_millis": 4 * 1000,
                "memory_bytes": 16 * GiB,
                "gpu": {"count": 1, "model": "NVIDIA H100 80GB HBM3"},
                "cpu_features": ["avx2", "avx512f"],
            },
            "runtime": {"agent": {
                # A tuple joined to another.
                "command": {"argv": ("/usr/local/bin/agent",) + ("--quiet",), "cwd": "/workspace"},
                "dirs": [{"path": "/workspace/notes", "mode": "0700"}],
                "files": [{
                    "path": "/workspace/notes/escapes.txt",
                    "mode": "0644",
                    # A tab, a quote, `A` itself and in octal, `é` and an emoji, the other control
                    # escapes, and a joined line.
                    "content": 'tab\there, quote \' and A\101é😀, \a\b\f\r\v, \
joined\n',
                }],
                # A string joined to another.
                "stdin": "Solve the task in /task/README.md, " + """then submit with "sandboxctl".
""",
                # Floored in each sign, as Starlark's `//` and `%` are: each term in parentheses is
                # an operation less what it gives, 0, so this is 5 hours.
                "deadline_seconds": 5 * 3600 + (7 // 2 - 3) + (-7 // 2 - -4) + (7 // -2 - -4)
                    + (-6 // 2 - -3) + (-7 % 3 - 2) + (7 % -4 - -1) + (6 % -3 - 0)
                    + ((-9223372036854775807 - 1) % -1 - 0),
                "finalize_grace_seconds": 60,
            }},
            "egress": {"model": [{"api": "responses", "models": ["model-large", "model-small"]}]},
            "grants": [
                {"judge": {
                    "name": "train",
                    "role": "train_judge",
                    "slot": "payload",
                    "calls": 100,
                    "cumulative_seconds": None,
                    "call_timeout_seconds": 10 * 60,
                }},
                {"submit": {"name": "final"}},
            ],
        },
        {
            "name": "train_judge",
            "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
            "runtime": {"judge": {
                "command": {
                    "argv": ("/opt/judge/run",),
                    "cwd": "/pumpkin/home",
                    "env": ["MODE=train", r"PATTERN=\d+\.json"],
                },
                # A hex int's digits may start with `0`, as a decimal int's can't.
                "slots": [{"name": "payload", "max_bytes": 0x010 * KiB}],
                # A list joined to another.
                "result": {"shows": {"score": "/score"}, "returns": ["/score"] + ["/metrics", "/failure"]},
            }},
        },
        {
            "name": "final_judge",
            "image": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "user": {"uid": 65532, "gid": 65532},
            "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
            "runtime": {"judge": {
                "command": {"argv": ["/opt/judge/run"], "cwd": "/pumpkin/home", "env": ["MODE=eval"]},
                "slots": [{"name": "payload", "max_bytes": 0o20 * KiB}],
                "result": {"shows": {"score": "/score"}, "returns": ["/score"]},
            }},
        },
    ],
    score = {"role": "final_judge", "slot": "payload", "timeout_seconds": 120},
)
