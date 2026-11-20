"""An in-memory board and its local client. The workflow supplies the caller as an argument."""

import json
import socket
import socketserver
import sys
import time

SOCKET = "board.sock"
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
    return {"id": ident}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            request = json.loads(self.rfile.readline())
            reply = answer(self.server.messages, self.server.roles, *request)
        except (ValueError, RecursionError) as exc:
            reply = {"error": str(exc)}
        self.wfile.write(encode(reply) + b"\n")


def call(op, caller, *args):
    try:
        text = sys.stdin.buffer.read().decode("utf-8")
    except UnicodeDecodeError:
        return {"error": "message must be UTF-8 text"}
    wire = encode([caller, op, args, text]) + b"\n"
    with socket.socket(socket.AF_UNIX) as client:
        # The first body can start before the service has bound its socket.
        until = time.monotonic() + 3
        while True:
            try:
                client.connect(SOCKET)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.monotonic() >= until:
                    raise
                time.sleep(0.05)
        client.sendall(wire)
        with client.makefile("rb") as response:
            return json.loads(response.readline(LIMIT + 1))


if __name__ == "__main__":
    if sys.argv[1] == "serve":
        with socketserver.UnixStreamServer(SOCKET, Handler) as service:
            service.roles = sys.argv[2:]
            service.messages = []
            service.serve_forever()
    else:
        sys.stdout.buffer.write(encode(call(*sys.argv[1:])))
