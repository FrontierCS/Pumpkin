"""The smoke's final call: it runs the smoke's judge on the submission in its target."""

def final_call(target: Target, submissions):
    """Shows how many lines of the submission's and of the judge's own probe say `ok`.

    Args:
      target: the call's target, whose image holds the smoke's judge.
      submissions: the agent’s submitted probe output by role, or None if absent.

    Returns:
      The score, the count of those lines, and the detail, every line of them.
    """
    payload = submissions["agent"]
    if payload == None:
        return {"score": 0, "detail": []}
    target.put("/pumpkin/in/payload/raw", payload)
    target.run(["python3", "-u", "/opt/smoke/judge.py"])
    result = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": result["score"], "detail": result["detail"]}
