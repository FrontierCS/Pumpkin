"""The hello example's final call: it counts the greeting's bytes and lines in its target."""

def final_call(target: Target, selections) -> dict[str, int]:
    """Shows the greeting's size as its score, and its lines.

    Args:
      target: the call's target.
      selections: the greeting by agent role, or None if absent.

    Returns:
      The score and the lines, in the order the run shows them.
    """
    payload = selections["agent"]
    if payload == None:
        return {"score": 0, "lines": 0}
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"score\": %s, \"lines\": %s}' $(wc -c < /workspace/greeting) $(wc -l < /workspace/greeting) > /pumpkin/out/result.json"])
    result = json.decode(target.get("/pumpkin/out/result.json"))
    return {"score": result["score"], "lines": result["lines"]}
