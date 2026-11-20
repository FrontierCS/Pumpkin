"""Alice posts one prediction. Bob reads it, checks it and adds his own."""

import json
import sys
import time

from client import board, call


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
