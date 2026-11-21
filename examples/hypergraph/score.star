"""The evaluator and final call's shared score."""

def score(target: Target, payload: Payload) -> dict[str, int]:
    """Scores the submitted core numbers in the judge's sandbox.

    Args:
      target: a fresh sandbox containing the graph and the judge.
      payload: the submitted JSON vector.

    Returns:
      A score of one for an exact vector and zero otherwise.
    """
    return json.decode(target.capture(["python3", "/opt/hypergraph/judge.py"], stdin = payload))

def final_score(target: Target, submissions) -> dict[str, int]:
    """Scores the agent's final submission.

    Args:
      target: the judge's sandbox.
      submissions: each role's payload, or None if it is absent.

    Returns:
      The score, zero if the agent submitted nothing.
    """
    payload = submissions["agent"]
    if payload == None:
        return {"score": 0}
    return score(target, payload)
