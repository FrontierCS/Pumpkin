"""The agent's submissions."""

def submit(run: Run, payload: Payload, finalize: bool = False) -> dict[str, str]:
    """Sets the calling agent's submission, replacing its previous payload.

    Args:
      run: the run's state.
      payload: the agent's submission.
      finalize: whether the call also locks the submission and ends the agent's work.

    Returns:
      `stored`, or `accepted` if the call locks the submission.
    """
    run.set_submission(payload)
    if finalize:
        run.end_work()
        return {"state": "accepted"}
    return {"state": "stored"}
