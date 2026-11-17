"""The agent's submissions: `submit` selects the payload that the final call judges."""

def submit(run: Run, payload: Payload, finalize: bool = False):
    """Selects `payload`, replacing the last, and, if `finalize`, also ends the agent's work."""
    run.select(payload)
    if finalize:
        run.end_work()
        return {"state": "accepted"}
    return {"state": "stored"}
