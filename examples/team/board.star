"""Shared posts and private messages between named participants."""

def _board(target, payload, op, args = []):
    reply = target.capture(["python3", "/opt/team/board.py", op, target.caller()] + args, stdin = payload)
    return json.decode(reply)

def post(target: Target, payload: Payload):
    """Publishes one message to the board.

    Args:
      target: the shared board.
      payload: the caller's UTF-8 message.

    Returns:
      The message ID or a request error.
    """
    return _board(target, payload, "post")

def send(target: Target, payload: Payload, to: str):
    """Delivers one message to a teammate.

    Args:
      target: the shared board.
      payload: the caller's UTF-8 message.
      to: the recipient's role name.

    Returns:
      The message ID or a request error.
    """
    return _board(target, payload, "send", [to])

def read(target: Target, payload: Payload, stream: str = "board", after: int = 0):
    """Reads one page visible to the caller.

    Args:
      target: the shared board.
      payload: empty.
      stream: board for public posts or inbox for private messages.
      after: the last message ID already read.

    Returns:
      Matching messages and the next cursor, or a request error.
    """
    return _board(target, payload, "read", [stream, str(after)])

def _grant(name, body, calls, help):
    return {"name": name, "help": help, "job": {
        "target": "board",
        "body": body,
        "payload_max_bytes": 2 * KiB,
        "call_timeout_seconds": 10,
        "calls": calls,
    }}

BOARD_GRANTS = [
    _grant("post", post, 50, "Publish the file's nonempty UTF-8 text to the shared board. Returns its message id."),
    _grant("send", send, 50, "Send the file's nonempty UTF-8 text to another role's inbox. Returns its message id."),
    _grant("read", read, 100, "Read public posts from board or private messages from your inbox. Omit the file. Returns messages with id, from and text, and an after cursor for the next page."),
]

def board(participants, user):
    """The work-scoped board for these named participants.

    Args:
      participants: the names allowed to send and receive messages.
      user: the account that owns the board inside its sandbox.

    Returns:
      The target role for the board grants.
    """
    return {
        "name": "board",
        "image": image("board/Dockerfile", context = "//team"),
        "user": user,
        "needs": {"cpu_millis": 500, "memory_bytes": 256 * MiB},
        "runtime": {"target": {
            "lifetime": "work_phase",
            "cwd": "/pumpkin/out",
            "env": ["TEAM_ROLES=" + " ".join(participants)],
        }},
    }
