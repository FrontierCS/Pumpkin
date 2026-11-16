"""A neutral library: `judged_task` writes the workflow of a task whose agent may call a judge
while it works, and whose final judge, of the same image, scores what it submits."""

AGENT_IMAGE = "sha256:1111111111111111111111111111111111111111111111111111111111111111"

JUDGE_IMAGE = "sha256:2222222222222222222222222222222222222222222222222222222222222222"

# What a judge needs, unless a task gives a value of its own.
JUDGE_NEEDS = {"cpu_millis": 1000, "memory_bytes": 512 * MiB}

# The mode each judge runs in.
MODES = {"train_judge": "train", "final_judge": "eval"}

def _judge(name, metrics, *, needs, env, max_bytes = 64 * KiB):
    """The judge role `name`, which returns its score, each of `metrics` but those that start
    with `_`, which the judge keeps to itself, and its failure."""
    mode = MODES[name]
    return {
        "name": name,
        "image": JUDGE_IMAGE,
        "user": {"uid": 65532, "gid": 65532},
        "needs": JUDGE_NEEDS | needs,
        "runtime": {"judge": {
            "command": {
                "argv": ["/opt/judge/run", "--mode", mode],
                "cwd": "/pumpkin/home",
                "env": [key + "=" + value for key, value in env.items()] + [
                    "MODE={mode}".format(mode = mode),
                    "SLOT_BYTES=" + str(max_bytes),
                ],
            },
            "slots": [{"name": "payload", "max_bytes": max_bytes}],
            "result": {
                "shows": {"score": "/score"},
                "returns": ["/score"] + [
                    "/metrics/" + metric
                    for metric in metrics
                    if not metric.startswith("_")
                ] + ["/failure"],
            },
        }},
    }

def _calls(calls):
    """How many train calls the agent may make: `calls`, or 10 if it is `None`."""
    if calls == None:
        return 10
    elif calls < 1:
        fail("`calls` is {}: give at least 1".format(calls))
    else:
        return calls

def judged_task(*, task, agent, metrics, calls = None, judge = {}, env = {}, models = []):
    """The workflow of the task at `task`, a path such as `tasks/<id>`: an agent with `agent`'s
    needs and the routes to `models`, a train judge that returns `metrics`, and a final judge
    that scores, each judge with `judge`'s needs and the variables of `env`."""
    id = task.split("/")[-1]
    egress = {"egress": {"model": [{"api": "responses", "models": sorted(models)}]}} if models else {}
    workflow(
        name = "neutral/" + id.replace("_", "-"),
        roles = [
            {
                "name": "agent",
                "image": AGENT_IMAGE,
                "user": {"uid": 1000, "gid": 1000},
                "needs": agent,
                "runtime": {"agent": {
                    "command": {"argv": ["/usr/local/bin/agent"], "cwd": "/workspace"},
                    "stdin": "Solve {}, scored by {} metrics: {}.\n".format(
                        id,
                        len(metrics),
                        ", ".join(metrics),
                    ),
                    "deadline_seconds": 5 * 3600,
                    "finalize_grace_seconds": int(env.get("GRACE", "60")),
                }},
                "grants": [
                    {"judge": {
                        "name": "train",
                        "role": "train_judge",
                        "slot": "payload",
                        "calls": _calls(calls),
                        "call_timeout_seconds": 600,
                    }},
                    {"submit": {"name": "final"}},
                ],
            } | egress,
            _judge("train_judge", metrics, needs = judge, env = env),
            _judge("final_judge", [], needs = judge, env = env, max_bytes = MiB),
        ],
        score = {"role": "final_judge", "slot": "payload", "timeout_seconds": 120},
    )
