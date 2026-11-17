"""The smoke's final call: it runs the smoke's judge on the submission in its target."""

def final_call(target: Target, payload: Payload):
    """Shows how many lines of the submission's and of the judge's own probe say `ok`.

    Args:
      target: the call's target, whose image holds the smoke's judge.
      payload: what the agent's probes printed, which the agent submitted.

    Returns:
      The score, the count of those lines, and the detail, every line of them.
    """
    target.put("/pumpkin/in/payload/raw", payload)
    target.run(["python3", "-u", "/opt/smoke/judge.py"])
    result = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": result["score"], "detail": result["detail"]}
