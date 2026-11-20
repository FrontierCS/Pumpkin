"""Call the board grant and wait for its reply."""

import json
import subprocess
import sys
import time
from pathlib import Path

CTL = "/.pumpkin/bin/sandboxctl"
PAYLOAD = Path("request.json")


def ctl(*args):
    reply = subprocess.run([CTL, *args], capture_output=True, text=True, check=False)
    if not reply.stdout:
        raise RuntimeError(reply.stderr)
    return json.loads(reply.stdout)


def call(grant, data, *args):
    PAYLOAD.write_text(json.dumps(data))
    return ctl("call", grant, str(PAYLOAD), *args)


def board(**request):
    until = time.monotonic() + 15
    while True:
        reply = call("board", request)
        if reply["kind"] == "call":
            break
        if (
            "this call is not charged" not in reply.get("message", "")
            or time.monotonic() > until
        ):
            raise RuntimeError(reply)
        time.sleep(0.1)
    while time.monotonic() < until:
        status = ctl("status", str(reply["job"]))
        if status.get("state") == "completed":
            return status["returned"]
        if status.get("state") not in {"running", "queued"}:
            raise RuntimeError(status)
        time.sleep(0.1)
    raise TimeoutError(request)


if __name__ == "__main__":
    print(json.dumps(board(**json.loads(sys.argv[1]))))
