"""Per-role selection and native training and final evaluation."""

def select(run: Run, payload: Payload):
    """Saves this role's archive.

    Args:
      run: the issuing role's run handle.
      payload: the exact native submission archive.

    Returns:
      A receipt for the saved selection.
    """
    run.set_submission(payload)
    return {"state": "stored"}

def finish(run: Run, _payload: Payload):
    """Locks this role's last saved archive.

    Args:
      run: the issuing role's run handle.
      _payload: the unused finish request.

    Returns:
      A receipt for the locked selection.
    """
    run.end_work()
    return {"state": "accepted"}

_TRAIN_RESULT = "/pumpkin/out/result.json"

def _train_worker(runner, target, directory):
    runner.put(directory, "/pumpkin/home/submission")
    command = runner.run(["python3", "/opt/team/judge.py", "train", "/pumpkin/home/submission/raw", _TRAIN_RESULT])
    if command["end"] != {"exited": {"code": 0}}:
        fail("native training command failed: " + json.encode(command["end"]))
    runner.get(_TRAIN_RESULT, "/pumpkin/in/feedback")
    return json.decode(target.get("/pumpkin/in/feedback/result.json"))

def train(target: Target, payload: Payload):
    """Returns the native train-visible results.

    Args:
      target: the fresh training verifier.
      payload: the archive to evaluate.

    Returns:
      The fields the native verifier publishes in train mode.
    """
    directory = "/pumpkin/in/payload"
    target.put(directory + "/raw", payload)
    return target.with_runner("native_train", _train_worker, target, directory)

def _put_selection(target, role, payload):
    if payload == None:
        return None
    path = "/pumpkin/in/" + role + ".tar"
    target.put(path, payload)
    return path

def final(target: Target, submissions):
    """Evaluates each role's chosen archive and takes the best individual score.

    Args:
      target: the fresh final verifier.
      submissions: each agent role's selected payload, or None.

    Returns:
      Each role's outcome and the best verified score.
    """
    paths = {role: _put_selection(target, role, payload) for role, payload in submissions.items()}
    reply = "/pumpkin/out/result.json"
    target.run(["python3", "/opt/team/judge.py", "eval", json.encode(paths), reply])
    return json.decode(target.get(reply))
