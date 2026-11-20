"""Alice posts one prediction. Bob reads it, checks it and adds his own."""

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


def select(role, claims, finalize):
    reply = call("select_" + role, claims, "--arg", "finalize=" + str(finalize).lower())
    if reply.get("returned", {}).get("state") != ("accepted" if finalize else "stored"):
        raise RuntimeError(reply)


def main(role, omit):
    peer = "bob" if role == "alice" else "alice"
    if call("select_" + peer, {})["kind"] != "refused":
        raise RuntimeError("peer's selection grant was accepted")
    if role == "alice":
        if "error" not in board(op="post", text="test", **{"from": peer}):
            raise RuntimeError("board accepted a forged sender")
        claims = {"amber": 4}
        if omit != "all":
            select(role, {}, False)
        receipt = board(op="post", text=json.dumps(claims))
        if "id" not in receipt:
            raise RuntimeError(receipt)
        if omit != "all":
            select(role, claims, True)
    else:
        until = time.monotonic() + 40
        while time.monotonic() < until:
            page = board(op="read", stream="board", after=0)
            if page["messages"]:
                break
            time.sleep(0.25)
        else:
            raise TimeoutError("Alice's prediction")
        (first,) = page["messages"]
        claims = json.loads(first["text"])
        if first["from"] != peer or claims != {"amber": (3 * 2 + 4 * 3 - 1) % 13}:
            raise ValueError(first)
        receipt = board(op="send", to=peer, text=f"Checked message {first['id']}")
        if "id" not in receipt:
            raise RuntimeError(receipt)
        claims["blue"] = (2 * 2 + 5 * 3 + 1) % 13
        if omit == "none":
            select(role, claims, True)


if __name__ == "__main__":
    main(*sys.argv[1:])
