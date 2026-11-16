"""The body of the agent's job function `count`."""

def count(target: Target, payload: Payload) -> dict[str, int]:
    """Counts the payload's lines and bytes with `wc`, in a fresh sandbox of the counter.

    Args:
      target: the counter's sandbox.
      payload: the greeting.

    Returns:
      The greeting's number of lines and of bytes.
    """
    target.put("/workspace/greeting", payload)
    target.run(["sh", "-c", "printf '{\"lines\": %s, \"bytes\": %s}' $(wc -l < greeting) $(wc -c < greeting) > counts.json"])
    return json.decode(target.get("/workspace/counts.json"))
