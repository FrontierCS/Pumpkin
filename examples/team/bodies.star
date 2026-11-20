"""Calls that drive the board and judge."""

def board_call(target: Target, payload: Payload):
    """Returns the board's reply to the authenticated caller.

    Args:
      target: the board sandbox shared by both agents.
      payload: the request issued by the caller.

    Returns:
      The committed message ID, the requested messages, or a request error.
    """
    request = "/pumpkin/in/request"
    response = "/pumpkin/home/response"
    target.run(["rm", "--force", request, response])
    target.put(request, payload)
    target.run(["python3", "/opt/team/board.py", "call", target.caller(), request, response])
    return json.decode(target.get(response))

def evaluate(target: Target, payload: Payload):
    """Compiles a draft source in a fresh runner and scores the resulting program.

    Args:
      target: the trusted evaluator with a compiler session.
      payload: the draft C++ source.

    Returns:
      The draft's status and score.
    """
    path = "/pumpkin/in/draft.cpp"
    result = "/pumpkin/out/result.json"
    target.put(path, payload)
    target.run(["python3", "/opt/team/judge.py", "draft", result, path])
    return json.decode(target.get(result))

def _stage(target, role, chosen):
    if chosen == None:
        return [role, ""]
    path = "/pumpkin/in/" + role + ".cpp"
    target.put(path, chosen)
    return [role, path]

def judge(target: Target, selections):
    """Scores each selected program and returns the best individual score.

    Args:
      target: the trusted judge with a compiler session.
      selections: each role's selected C++ source, or None for a missing submission.

    Returns:
      The team score and each role's source digest, status and score.
    """
    result = "/pumpkin/out/result.json"
    args = [arg for role, chosen in selections.items() for arg in _stage(target, role, chosen)]
    target.run(["python3", "/opt/team/judge.py", "final", result] + args)
    return json.decode(target.get(result))
