"""A neutral workflow whose parts a library gives."""

load(
    "//lib/parts.star",
    "AGENT",
    "JUDGE_IMAGE",
    "JUDGE_USER",
    ending = "FINAL",
)

workflow(
    deadline_seconds = 3600,
    name = "neutral/loaded",
    roles = [
        AGENT,
        {
            "name": "final_judge",
            "image": JUDGE_IMAGE,
            "user": JUDGE_USER,
            "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
            "runtime": {"target": {"cwd": "/pumpkin/home"}},
        },
    ],
    final = ending,
)
