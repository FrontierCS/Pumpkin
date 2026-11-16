"""A neutral library: `images_task` writes the workflow of a task whose package's recipes build
its images: the agent's, on the package's build tools with its toolchain copied in, with its
harness added on it, and the trainer's and the judge's, each a stage of one recipe."""

# The run's choices, each given as `--arg <name>=<value>`, or else its default.
MODEL = arg("model", str, default = "model-small")
DEADLINE = arg("deadline", int, default = 5 * 3600)
STRICT = arg("strict", bool, default = False)

# The directories each recipe copies from, beside its package.
SHARED = {"shared": "//shared"}

# The build arguments of the recipes that start from the shared base, which they name `BASE`.
BASE_ARGS = {
    "BASE": "docker.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
}

# The judge's runtime: it scores the payload in its slot.
JUDGE = {"judge": {
    "command": {"argv": ["/opt/judge/run"], "cwd": "/pumpkin/home"},
    "slots": [{"name": "payload", "max_bytes": 64 * KiB}],
    "result": {"shows": {"score": "/score"}, "returns": ["/score"]},
}}

def _role(name, stage, runtime):
    """The role `name`, on the image of `stage` of the package's judge recipe."""
    return {
        "name": name,
        "image": image("judge/Dockerfile", mount = SHARED, target = stage, args = BASE_ARGS),
        "user": {"uid": 65532, "gid": 65532},
        "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
        "runtime": runtime,
    }

def train(target: Target, payload: Payload):
    """Scores the payload by the judge's train stage, in a fresh sandbox of the trainer.

    Args:
      target: the trainer's sandbox.
      payload: the payload to score.

    Returns:
      The judge's result.
    """
    target.put("/pumpkin/in/payload", payload)
    target.run(["/opt/judge/run"])
    return json.decode(target.get("/pumpkin/out/result.json"))

def images_task(*, agent):
    """The workflow of the task in the entry's package: a harness works as the agent, with
    `agent`'s needs, on the image of the package's agent recipe, which calls the trainer, and a
    judge scores it."""
    task = package_name().split("/")[-1]
    tools = image("tools/Dockerfile", args = BASE_ARGS)
    toolchain = image("toolchain/Dockerfile", target = "Go")
    built_on = {"build-tools": tools, "toolchain": toolchain}
    agent_image = image("agent/Dockerfile", context = ".", mount = SHARED, contexts = built_on)
    # Declared between the agent's image and the layer on it, so that the layer's base is not the
    # image built just before it.
    stages = [
        _role("trainer", "train", {"target": {"cwd": "/pumpkin/home"}}),
        _role("final_judge", "eval", JUDGE),
    ]
    harness = image("Dockerfile", context = "//harness", contexts = {"base": agent_image})
    workflow(
        name = "neutral/" + task.replace("_", "-"),
        roles = [
            {
                "name": "agent",
                "image": harness,
                "user": {"uid": 1000, "gid": 1000},
                "needs": agent,
                "runtime": {"agent": {
                    "command": {"argv": ["/harness/bin/agent"], "cwd": "/workspace"},
                    "stdin": "Solve the task in /task/README.md, then submit with \"sandboxctl\".\n",
                    "deadline_seconds": DEADLINE,
                    "finalize_grace_seconds": 0 if STRICT else 60,
                    "egress": {"model": [{"api": "responses", "models": [MODEL]}]},
                    "grants": [
                        {"name": "train", "job": {
                            "target": "trainer",
                            "body": train,
                            "payload_max_bytes": 64 * KiB,
                            "calls": 10,
                            "call_timeout_seconds": 600,
                        }},
                        {"name": "final", "submit": {}},
                    ],
                }},
            },
        ] + stages,
        score = {"role": "final_judge", "slot": "payload", "timeout_seconds": 120},
    )
