"""A neutral library: `judged_task` writes the workflow of a task whose agent may call the judge's
train mode while it works, and whose final judge, of the same image, scores what it submits."""

load("//lib/submit.star", "submit")

AGENT_IMAGE = "sha256:1111111111111111111111111111111111111111111111111111111111111111"

JUDGE_IMAGE = "sha256:2222222222222222222222222222222222222222222222222222222222222222"

# What a judge needs, unless a task gives a value of its own.
JUDGE_NEEDS = {"cpu_millis": 1000, "memory_bytes": 512 * MiB}

def _env(env, mode, max_bytes):
    """The variables of `env`, then the judge's mode and the most bytes it reads."""
    return [key + "=" + value for key, value in env.items()] + [
        "MODE={mode}".format(mode = mode),
        "SLOT_BYTES=" + str(max_bytes),
    ]

def _trainer(*, needs, env):
    """The target that each train call starts, on the judge's image."""
    return {
        "name": "trainer",
        "image": JUDGE_IMAGE,
        "user": {"uid": 65532, "gid": 65532},
        "needs": JUDGE_NEEDS | needs,
        "runtime": {"target": {"cwd": "/pumpkin/home", "env": _env(env, "train", 64 * KiB)}},
    }

def _judge(*, needs, env, max_bytes):
    """The final judge, which returns its score and its failure."""
    return {
        "name": "final_judge",
        "image": JUDGE_IMAGE,
        "user": {"uid": 65532, "gid": 65532},
        "needs": JUDGE_NEEDS | needs,
        "runtime": {"judge": {
            "command": {
                "argv": ["/opt/judge/run", "--mode", "eval"],
                "cwd": "/pumpkin/home",
                "env": _env(env, "eval", max_bytes),
            },
            "slots": [{"name": "payload", "max_bytes": max_bytes}],
            "result": {"shows": {"score": "/score"}, "returns": ["/score", "/failure"]},
        }},
    }

def train(target: Target, payload: Payload, *, metrics: list[str]):
    """Runs the judge in train mode on the payload.

    Args:
      target: the trainer's sandbox.
      payload: the payload to judge.
      metrics: the result's metrics to return.

    Returns:
      The judge's score, and its metrics that `metrics` names.
    """
    target.put("/pumpkin/in/payload", payload)
    target.run(["/opt/judge/run", "--mode", "train"])
    found = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": found["score"]} | {name: found["metrics"][name] for name in metrics}

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
    needs and the routes to `models`, a train function whose calls return `metrics` but those that
    start with `_`, which the judge keeps to itself, and a final judge that scores, the trainer and
    the judge each with `judge`'s needs and the variables of `env`."""
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
                    "grants": [
                        {"name": "train", "job": {
                            "target": "trainer",
                            "body": train,
                            "keywords": {
                                "metrics": [metric for metric in metrics if not metric.startswith("_")],
                            },
                            "payload_max_bytes": 64 * KiB,
                            "calls": _calls(calls),
                            "call_timeout_seconds": 600,
                        }},
                        {"name": "final", "now": {"body": submit, "payload_max_bytes": MiB}},
                    ],
                } | egress},
            },
            _trainer(needs = judge, env = env),
            _judge(needs = judge, env = env, max_bytes = MiB),
        ],
        score = {"role": "final_judge", "slot": "payload", "timeout_seconds": 120},
    )
