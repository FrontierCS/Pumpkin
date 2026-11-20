"""The smoke's judge: runs the probe here, and scores each line of its own and of the
submission's that says `ok`; its result's `detail` holds every line."""

import json
import subprocess

with open("/pumpkin/in/payload/raw") as file:
    lines = file.read().splitlines()
probe = ["python3", "-u", "/opt/smoke/probe.py", "judge"]
lines += subprocess.run(
    probe, check=False, capture_output=True, text=True
).stdout.splitlines()
score = sum(1 for line in lines if ": ok (" in line)
with open("/pumpkin/out/result.json", "w") as file:
    json.dump({"score": score, "detail": lines}, file)
