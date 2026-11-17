"""A job function's body: it runs the verifier on the payload in the call's target."""

def verify(target: Target, payload: Payload, *, metrics: list[str]):
    """Verifies the payload.

    Args:
      target: the call's target.
      payload: the payload to verify.
      metrics: the result's metrics to return.

    Returns:
      The result's metrics that `metrics` names.
    """
    target.put("/pumpkin/in/payload", payload)
    target.run(["/opt/verifier"])
    found = json.decode(target.get("/pumpkin/out/result.json"))
    return {name: found[name] for name in metrics if name in found}
