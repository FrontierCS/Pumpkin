"""The codex example's final call: it counts the greeting's bytes in its target."""

def final_call(target: Target, payload: Payload) -> dict[str, int]:
    """Shows the greeting's size as its score.

    Args:
      target: the call's target.
      payload: the greeting the agent submitted.

    Returns:
      The score.
    """
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"score\": %s}' $(wc -c < /workspace/greeting) > /pumpkin/out/result.json"])
    return {"score": json.decode(target.get("/pumpkin/out/result.json"))["score"]}
