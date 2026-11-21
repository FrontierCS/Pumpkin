"""Five individual selections, one shared board and the native train and final grants."""

load("//hypergraph-benchmark/calls.star", "final", "finish", "select", "train")
load("//team/board.star", "BOARD_GRANTS", "board")

ROLES = ["alice", "bob", "carol", "dan", "eve"]
JUDGE_USER = {"uid": 65532, "gid": 65532}
JUDGE_NEEDS = {"cpu_millis": 8000, "memory_bytes": 4 * GiB}

_ARCHIVE = "Run make-team-archive with no arguments to package your current checkout. Pass the new archive path it prints as FILE."

GRANTS = BOARD_GRANTS + [
    {"name": "train", "help": "Evaluate a draft using the public training suite. " + _ARCHIVE + " Returns official training feedback.", "job": {"target": "train_judge", "body": train, "payload_max_bytes": 256 * KiB, "calls": 100, "cumulative_seconds": 5400, "call_timeout_seconds": 1840}},
    {"name": "select", "help": "Save the archive as your current selection. " + _ARCHIVE + " Replaces your previous selection. Returns its stored state.", "now": {"body": select, "payload_max_bytes": 256 * KiB}},
    {"name": "finish", "help": "Lock your last selection and finish work. Omit the file. Returns its accepted state.", "now": {"body": finish, "payload_max_bytes": 64}},
]

def team(agents, train_image, judge_image):
    """Declare the shared board and native train and final targets for these agents.

    Args:
      agents: the five declared participant roles.
      train_image: the development verifier with the feedback workloads.
      judge_image: the pinned native verifier image.
    """
    workflow(
        name = "examples/hypergraph-benchmark",
        deadline_seconds = 5400,
        roles = agents + [
            board(ROLES, JUDGE_USER),
            {
                "name": "train_judge",
                "image": image("board/Dockerfile", context = "//team"),
                "user": JUDGE_USER,
                "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
                "runtime": {"target": {
                    "lifetime": "call",
                    "cwd": "/pumpkin/home",
                    "sessions": [{
                        "name": "native_train",
                        "session": {
                            "runner": "train_worker",
                            "instances": 1,
                            "instance_seconds": 1840,
                            "commands": 3,
                            "transfer_bytes": 1 * MiB,
                        },
                    }],
                }},
            },
            {
                "name": "train_worker",
                "image": train_image,
                "user": JUDGE_USER,
                "needs": JUDGE_NEEDS,
                "runtime": {"runner": {"cwd": "/pumpkin/home"}},
            },
            {
                "name": "judge",
                "image": judge_image,
                "user": JUDGE_USER,
                "needs": JUDGE_NEEDS,
                "runtime": {"target": {"lifetime": "call", "cwd": "/pumpkin/home"}},
            },
        ],
        final = {"target": "judge", "body": final, "payload_max_bytes": 256 * KiB, "timeout_seconds": 9200},
    )
