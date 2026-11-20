"""The codex example's final call: it counts the greeting's bytes in its target."""

def final_call(target: Target, selections) -> dict[str, int]:
    """Shows the greeting's size as its score.

    Args:
      target: the call's target.
      selections: the greeting by agent role, or None if absent.

    Returns:
      The score.
    """
    payload = selections["agent"]
    if payload == None:
        return {"score": 0}
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"score\": %s}' $(wc -c < /workspace/greeting) > /pumpkin/out/result.json"])
    return {"score": json.decode(target.get("/pumpkin/out/result.json"))["score"]}
