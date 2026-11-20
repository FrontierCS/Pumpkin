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

def judge(target: Target, payload: Payload):
    """Scores every role's predictions and returns the greatest individual score.

    Args:
      target: the fresh judge sandbox containing each present selection.
      payload: the driver’s ordered manifest of roles and selected files.

    Returns:
      Each role's score and their maximum.
    """
    manifest = "/pumpkin/in/manifest"
    result = "/pumpkin/out/result.json"
    target.put(manifest, payload)
    target.run(["python3", "/opt/team/judge.py", manifest, result])
    return json.decode(target.get(result))
