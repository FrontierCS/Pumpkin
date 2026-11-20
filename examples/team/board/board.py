"""Read or update the team board in its work-scoped sandbox."""

import json
import os
import sys
from pathlib import Path

BOARD = Path("board.json")
LIMIT = 16 * 1024


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def answer(messages, roles, caller, op, args, text):
    if caller not in roles:
        raise ValueError("unknown caller")
    if op == "read":
        stream, cursor = args
        after = int(cursor)
        if stream not in ("board", "inbox") or not 0 <= after <= len(messages):
            raise ValueError(
                "read needs board or inbox and a cursor returned by the service (initially 0)"
            )
        page = {"messages": [], "after": after}
        destination = None if stream == "board" else caller
        for message in messages:
            if message["id"] > after and message.get("to") == destination:
                trial = {
                    "messages": [*page["messages"], message],
                    "after": message["id"],
                }
                if len(encode(trial)) > LIMIT:
                    break
                page = trial
        return page
    if op == "post":
        fields = {}
    elif op == "send":
        (recipient,) = args
        if recipient not in roles or recipient == caller:
            raise ValueError("send needs a different named recipient")
        fields = {"to": recipient}
    else:
        raise ValueError("unexpected operation or fields")
    if not text:
        raise ValueError("message is empty")
    ident = len(messages) + 1
    messages.append({"id": ident, "from": caller, **fields, "text": text})
    pending = BOARD.with_suffix(".tmp")
    pending.write_bytes(encode(messages))
    pending.replace(BOARD)
    return {"id": ident}


def call(op, caller, *args):
    try:
        messages = json.loads(BOARD.read_bytes())
    except FileNotFoundError:
        messages = []
    try:
        text = sys.stdin.buffer.read().decode("utf-8")
    except UnicodeDecodeError:
        return {"error": "message must be UTF-8 text"}
    try:
        roles = os.environ["TEAM_ROLES"].split()
        return answer(messages, roles, caller, op, args, text)
    except ValueError as exc:
        return {"error": str(exc)}


if __name__ == "__main__":
    sys.stdout.buffer.write(encode(call(*sys.argv[1:])))
