"""A neutral task, whose workflow a library writes from its values."""

load("//lib/judged.star", "judged_task")

judged_task(
    task = "tasks/judged_neutral",
    agent = {"cpu_millis": 4000, "memory_bytes": 8 * GiB},
    metrics = ["solved", "_internal", "rejected"],
    calls = 20,
    judge = {"memory_bytes": GiB},
    env = {"LEVEL": "hard", "GRACE": "30"},
    models = ["model-small", "model-large"],
)
