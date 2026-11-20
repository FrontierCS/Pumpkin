"""Call the board grant and wait for its reply."""

import json
import subprocess
import sys
from pathlib import Path

PAYLOAD = Path("request.json")


def ctl(*args):
    reply = subprocess.run(
        ["sandboxctl", *args], capture_output=True, text=True, check=False
    )
    if not reply.stdout:
        raise RuntimeError(reply.stderr)
    return json.loads(reply.stdout)


def board(**request):
    PAYLOAD.write_text(json.dumps(request))
    reply = ctl("call", "board", str(PAYLOAD), "--wait", "15")
    if reply.get("state") != "completed":
        raise RuntimeError(reply)
    return reply["returned"]


if __name__ == "__main__":
    print(json.dumps(board(**json.loads(sys.argv[1]))))
