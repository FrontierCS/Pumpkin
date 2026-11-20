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


def answer(messages, roles, caller, request):
    if caller not in roles or not isinstance(request, dict):
        raise ValueError("unknown caller or invalid request")
    op = request.get("op")
    if op == "read" and request.keys() == {"op", "stream", "after"}:
        stream, after = request["stream"], request["after"]
        if (
            stream not in ("board", "inbox")
            or type(after) is not int
            or not 0 <= after <= len(messages)
        ):
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
    if op == "post" and request.keys() == {"op", "text"}:
        fields = {}
    elif op == "send" and request.keys() == {"op", "to", "text"}:
        recipient = request["to"]
        if recipient not in roles or recipient == caller:
            raise ValueError("send needs a different named recipient")
        fields = {"to": recipient}
    else:
        raise ValueError("unexpected operation or fields")
    text = request["text"]
    if not isinstance(text, str) or not 0 < len(text.encode("utf-8")) <= 2048:
        raise ValueError("message must be 1 to 2048 UTF-8 bytes")
    ident = len(messages) + 1
    messages.append({"id": ident, "from": caller, **fields, "text": text})
    return {"id": ident}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        try:
            caller, request = json.loads(self.rfile.readline())
            reply = answer(self.server.messages, self.server.roles, caller, request)
        except (ValueError, RecursionError) as exc:
            reply = {"error": str(exc)}
        self.wfile.write(encode(reply) + b"\n")


def call(caller):
    try:
        request = json.load(sys.stdin.buffer)
        wire = encode([caller, request]) + b"\n"
    except (ValueError, RecursionError) as exc:
        return {"error": str(exc)}
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
        sys.stdout.buffer.write(encode(call(sys.argv[2])))
