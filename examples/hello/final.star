"""The hello example's final call: it counts the greeting's bytes and lines in its target."""

def final_call(target: Target, payload: Payload) -> dict[str, int]:
    """Shows the greeting's size as its score, and its lines.

    Args:
      target: the call's target.
      payload: the greeting the agent submitted.

    Returns:
      The score and the lines, in the order the run shows them.
    """
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"score\": %s, \"lines\": %s}' $(wc -c < /workspace/greeting) $(wc -l < /workspace/greeting) > /pumpkin/out/result.json"])
    result = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": result["score"], "lines": result["lines"]}
