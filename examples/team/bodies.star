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

JUDGE = "/opt/team/judge.py"
COMPILE = "sha256sum src/solution.cpp && exec g++ --std=c++17 src/solution.cpp --output solution"

# The extra byte lets the judge reject an oversized answer, within the runner's stdout tail.
SOLVE = 'ulimit -f 1; printf "%s" "$1" | ./solution > answer && head --bytes=1025 answer'

def _case(target, runner, inputs, name):
    result = runner.run(["sh", "-c", SOLVE, "solve", inputs[name]], timeout_seconds = 3)
    if result["end"] != {"exited": {"code": 0}}:
        return None
    return json.decode(target.capture(["python3", JUDGE, "check", name, json.encode(result["stdout_tail"])]))

def _run_source(runner, target, directory, inputs):
    runner.put(directory, "/pumpkin/home/src")
    built = runner.run(["sh", "-c", COMPILE], timeout_seconds = 20)
    if built["end"] != {"exited": {"code": 0}}:
        return None
    repeated = _case(target, runner, inputs, "repeated_edge")
    if repeated == None:
        return None
    fixture = _case(target, runner, inputs, "fixture")
    return None if fixture == None else repeated + fixture

def _score_source(target, role, source):
    if source == None:
        return {"role": role, "digest": None, "status": "missing", "score": None}
    directory = "/pumpkin/in/" + role
    path = directory + "/solution.cpp"
    target.put(path, source)
    prepared = json.decode(target.capture(["python3", JUDGE, "prepare", path]))
    inputs = prepared["inputs"]
    points = target.with_runner("cpp", _run_source, target, directory, inputs) if inputs != None else None
    if points == None:
        return {"role": role, "digest": prepared["digest"], "status": "invalid", "score": 0}
    return {"role": role, "digest": prepared["digest"], "status": "scored" if points == 2 else "wrong", "score": points}

def evaluate(target: Target, payload: Payload):
    """Compiles and grades the caller's draft in a fresh runner.

    Args:
      target: the trusted evaluator with a compiler grant.
      payload: the draft C++ source.

    Returns:
      The draft's status and score.
    """
    row = _score_source(target, target.caller(), payload)
    return {"status": row["status"], "score": row["score"]}

def judge(target: Target, submissions):
    """Scores the selected programs, keeping a result row for every agent.

    Args:
      target: the trusted judge with a compiler grant.
      submissions: maps each agent role to its C++ payload or None.

    Returns:
      The highest score and the per-role scores, digests and statuses.
    """
    rows = [_score_source(target, role, source) for role, source in submissions.items()]
    result = "/pumpkin/out/result.json"
    target.run(["python3", JUDGE, "final", result, json.encode(rows)])
    return json.decode(target.get(result))
