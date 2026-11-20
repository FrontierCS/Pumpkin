"""The public prediction team, its board and its final judge."""

load("//submit.star", "submit")
load("//team/bodies.star", "board_call", "judge")

PARTICIPANTS = ["alice", "bob"]
USER = {"uid": 1000, "gid": 1000}
SERVICE_NEEDS = {"cpu_millis": 500, "memory_bytes": 256 * MiB}
GRANT = {"name": "board", "job": {
    "target": "board",
    "body": board_call,
    "payload_max_bytes": 16 * KiB,
    "call_timeout_seconds": 10,
    "calls": 200,
}}

def team(name, agent_image, runtimes, *, needs, deadline_seconds):
    """Runs Alice and Bob with access to the same board and judge.

    Args:
      name: the workflow name.
      agent_image: the image for each participant's separate sandbox.
      runtimes: each participant's agent runtime, keyed by role name.
      needs: the resources of each agent sandbox.
      deadline_seconds: the shared work deadline.
    """
    workflow(
        name = name,
        deadline_seconds = deadline_seconds,
        roles = [
            {
                "name": role,
                "image": agent_image,
                "user": USER,
                "needs": needs,
                "runtime": {"agent": runtimes[role] | {"grants": [
                    GRANT,
                    {"name": "select_" + role, "now": {"body": submit, "payload_max_bytes": 1 * KiB}},
                ]}},
            }
            for role in PARTICIPANTS
        ] + [
            {
                "name": "board",
                "image": image("board/Dockerfile"),
                "user": USER,
                "needs": SERVICE_NEEDS,
                "runtime": {"shared": {
                    "command": {"argv": ["python3", "/opt/team/board.py", "serve"] + PARTICIPANTS, "cwd": "/pumpkin/home"},
                    "cwd": "/pumpkin/home",
                }},
            },
            {
                "name": "judge",
                "image": image("judge/Dockerfile"),
                "user": USER,
                "needs": SERVICE_NEEDS,
                "runtime": {"target": {"cwd": "/pumpkin/home"}},
            },
        ],
        final = {"target": "judge", "body": judge, "input": "selections", "payload_max_bytes": 1 * KiB, "timeout_seconds": 30},
    )
