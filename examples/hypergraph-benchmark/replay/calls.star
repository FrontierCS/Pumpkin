"""Start the replay and return the official timing results."""

def start(run: Run, payload: Payload):
    run.set_submission(payload)
    run.end_work()
    return {"state": "accepted"}

def replay(target: Target, _submissions):
    target.run(["python3", "/opt/team/replay.py"])
    return json.decode(target.get("/pumpkin/out/result.json"))
