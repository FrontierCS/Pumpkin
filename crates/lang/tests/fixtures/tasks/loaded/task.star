"""A neutral workflow whose parts a library gives."""

load(
    "//lib/parts.star",
    "AGENT",
    "JUDGE_IMAGE",
    "JUDGE_USER",
    "SCORE",
    result = "RESULT",
)

workflow(
    name = "neutral/loaded",
    roles = [
        AGENT,
        {
            "name": "final_judge",
            "image": JUDGE_IMAGE,
            "user": JUDGE_USER,
            "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
            "runtime": {"judge": {
                "command": {"argv": ["/opt/judge/run"], "cwd": "/pumpkin/home"},
                "slots": [{"name": "payload", "max_bytes": 64 * KiB}],
                "result": result,
            }},
        },
    ],
    score = SCORE,
)
