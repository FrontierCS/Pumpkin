"""Calls that drive the board and judge."""

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

def evaluate(target: Target, payload: Payload):
    """Compiles a draft source in a fresh runner and scores the resulting program.

    Args:
      target: the trusted evaluator with a compiler session.
      payload: the draft C++ source.

    Returns:
      The draft's status and score.
    """
    reply = target.capture(["python3", "/opt/team/judge.py", "draft"], stdin = payload)
    return json.decode(reply)

def _stage(target, role, chosen):
    if chosen == None:
        return [role, ""]
    path = "/pumpkin/in/" + role + ".cpp"
    target.put(path, chosen)
    return [role, path]

def judge(target: Target, submissions):
    """Scores each selected program and returns the best individual score.

    Args:
      target: the trusted judge with a compiler session.
      submissions: maps each agent role to its C++ payload or None.

    Returns:
      The team score and each role's source digest, status and score.
    """
    result = "/pumpkin/out/result.json"
    args = [arg for role, chosen in submissions.items() for arg in _stage(target, role, chosen)]
    target.run(["python3", "/opt/team/judge.py", "final", result] + args)
    return json.decode(target.get(result))
