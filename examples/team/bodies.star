"""Calls that drive the board and judge."""

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
