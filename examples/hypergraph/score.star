"""The evaluator and final call's shared score."""

def score(target: Target, payload: Payload) -> dict[str, int]:
    """Scores the submitted core numbers in the judge's sandbox.

    Args:
      target: a fresh sandbox containing the graph and the judge.
      payload: the submitted JSON vector.

    Returns:
      A score of one for an exact vector and zero otherwise.
    """
    target.put("/pumpkin/in/payload/raw", payload)
    target.run(["python3", "/opt/hypergraph/judge.py", "/pumpkin/in/payload/raw", "/pumpkin/out/result.json"])
    return json.decode(target.get("/pumpkin/out/result.json"))

def final_score(target: Target, selections) -> dict[str, int]:
    """Scores the agent's final selection.

    Args:
      target: the judge's sandbox.
      selections: each role's payload, or None if it is absent.

    Returns:
      The score, zero if the agent submitted nothing.
    """
    payload = selections["agent"]
    if payload == None:
        return {"score": 0}
    return score(target, payload)
