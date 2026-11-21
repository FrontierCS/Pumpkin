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

def submission(name, payload_max_bytes):
    """The named grant to save and finalize this agent's file.

    Args:
      name: the role's public command name.
      payload_max_bytes: the largest file it may submit.

    Returns:
      The agent's now grant.
    """
    return {
        "name": name,
        "help": "Save the file as your current submission. Set finalize=true to lock it and end your work. Returns state stored, or accepted when finalized.",
        "now": {"body": submit, "payload_max_bytes": payload_max_bytes},
    }
