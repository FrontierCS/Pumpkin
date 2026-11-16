"""The agent's submissions."""

def submit(run: Run, payload: Payload, finalize: bool = False) -> dict[str, str]:
    """Selects `payload` for the final call, replacing the last.

    Args:
      run: the run's state.
      payload: the agent's submission.
      finalize: whether the call also ends the agent's work.

    Returns:
      The selection's state: `stored`, or `accepted` once the work has ended.
    """
    run.select(payload)
    if finalize:
        run.end_work()
        return {"state": "accepted"}
    return {"state": "stored"}
