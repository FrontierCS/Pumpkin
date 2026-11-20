"""Calls that drive the board and judge."""

def board_call(target: Target, payload: Payload):
    """Returns the board's reply to the authenticated caller.

    Args:
      target: the board sandbox shared by both agents.
      payload: the request issued by the caller.

    Returns:
      The committed message ID, the requested messages, or a request error.
    """
    reply = target.capture(["python3", "/opt/team/board.py", "call", target.caller()], stdin = payload)
    return json.decode(reply)

JUDGE = "/opt/team/judge.py"
COMPILE = "sha256sum src/solution.cpp && exec g++ --std=c++17 src/solution.cpp --output solution"

# The extra byte lets the judge reject an oversized answer, within the runner's stdout tail.
SOLVE = 'ulimit -f 1; printf "%s" "$1" | ./solution > answer && head --bytes=1025 answer'

def _answer(runner, graph):
    result = runner.run(["sh", "-c", SOLVE, "solve", graph], timeout_seconds = 3)
    return result["stdout_tail"] if result["end"] == {"exited": {"code": 0}} else None

def _run_source(runner, directory, inputs):
    runner.put(directory, "/pumpkin/home/src")
    built = runner.run(["sh", "-c", COMPILE], timeout_seconds = 20)
    if built["end"] != {"exited": {"code": 0}}:
        return None
    return [_answer(runner, graph) for graph in inputs]

def _score_source(target, role, source):
    if source == None:
        return {"role": role, "digest": None, "status": "missing", "score": None}
    directory = "/pumpkin/in/" + role
    path = directory + "/solution.cpp"
    target.put(path, source)
    prepared = json.decode(target.capture(["python3", JUDGE, "prepare", path]))
    inputs = prepared["inputs"]
    outputs = target.with_runner("cpp", _run_source, directory, inputs) if inputs != None else None
    grade = json.decode(target.capture(["python3", JUDGE, "grade", json.encode(outputs)]))
    return {"role": role, "digest": prepared["digest"]} | grade

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
