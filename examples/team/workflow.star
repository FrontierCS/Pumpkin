"""The hypergraph team, its board and its final judge."""

load("//submit.star", "submission")
load("//team/board.star", "BOARD_GRANTS", "board")
load("//team/bodies.star", "evaluate", "judge")

SOURCE_BYTES = 64 * KiB
PARTICIPANTS = ["alice", "bob"]
USER = {"uid": 1000, "gid": 1000}
TARGET_NEEDS = {"cpu_millis": 500, "memory_bytes": 256 * MiB}

def team(name, agent_image, runtimes, *, toolchain, needs, deadline_seconds, evaluations = 0):
    """Runs Alice and Bob with access to the same board and judge.

    Args:
      name: the workflow name.
      agent_image: the image for each participant's separate sandbox.
      runtimes: each participant's agent runtime, keyed by role name.
      toolchain: the compiler image shared by the client and judge runners.
      needs: the resources of each agent sandbox.
      deadline_seconds: the shared work deadline.
      evaluations: how many drafts each role may score before selecting.
    """
    evaluators = [("judge", "compiler")] + ([("evaluator", "eval_compiler")] if evaluations else [])
    draft = [{"name": "eval", "help": "Compile and score the file as draft C++ source. Returns its status and score.", "job": {
        "target": "evaluator",
        "body": evaluate,
        "calls": evaluations,
        "payload_max_bytes": SOURCE_BYTES,
        "call_timeout_seconds": 90,
    }}] if evaluations else []
    judge_image = image("judge/Dockerfile", mount = {"hypergraph": "//hypergraph"})
    workflow(
        name = name,
        deadline_seconds = deadline_seconds,
        roles = [
            {
                "name": role,
                "image": agent_image,
                "user": USER,
                "needs": needs,
                "runtime": {"agent": runtimes[role] | {"grants": draft + BOARD_GRANTS + [submission("submit", SOURCE_BYTES)]}},
            }
            for role in PARTICIPANTS
        ] + [
            board(PARTICIPANTS, USER),
        ] + [
            {
                "name": target,
                "image": judge_image,
                "user": USER,
                "needs": TARGET_NEEDS,
                "runtime": {"target": {"cwd": "/pumpkin/home", "sessions": [{
                    "name": "cpp",
                    "session": {"runner": compiler, "instances": 1, "instance_seconds": 60, "commands": 8, "transfer_bytes": SOURCE_BYTES},
                }]}},
            }
            for target, compiler in evaluators
        ] + [
            {
                "name": compiler,
                "image": toolchain,
                "user": USER,
                "needs": {"cpu_millis": 1000, "memory_bytes": 512 * MiB},
                "runtime": {"runner": {"cwd": "/pumpkin/home"}},
            }
            for _, compiler in evaluators
        ],
        final = {"target": "judge", "body": judge, "payload_max_bytes": SOURCE_BYTES, "timeout_seconds": 600},
    )
