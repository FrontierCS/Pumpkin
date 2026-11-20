"""Alice tests her degree baseline. Bob checks her graph with a peeling implementation."""

import json
import subprocess
import sys
import time
from pathlib import Path

SOLVER = Path("/opt/team/solver")
SOURCE = Path("solution.cpp")


def call(grant, *args):
    reply = subprocess.run(
        ["sandboxctl", "call", grant, *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if not reply.stdout:
        raise RuntimeError(reply.stderr)
    return json.loads(reply.stdout)


def board(grant, *args):
    reply = call(grant, *args, "--wait", "15")
    if reply.get("state") != "completed":
        raise RuntimeError(reply)
    return reply["returned"]


def select(path, finalize):
    reply = call("submit", str(path), "--arg", f"finalize={str(finalize).lower()}")
    if reply.get("returned", {}).get("state") != ("accepted" if finalize else "stored"):
        raise RuntimeError(reply)


def solve(vertices, edges):
    lines = [
        f"{vertices} {len(edges)}",
        *(" ".join(map(str, [len(edge), *edge])) for edge in edges),
    ]
    done = subprocess.run(
        ["./solution"],
        input="\n".join(lines) + "\n",
        text=True,
        capture_output=True,
        check=True,
    )
    return list(map(int, done.stdout.split()))


def main(role, omit):
    peer = "bob" if role == "alice" else "alice"
    body = "degrees.cpp" if role == "alice" else "peel.cpp"
    SOURCE.write_bytes(
        (SOLVER / "main.cpp").read_bytes() + (SOLVER / body).read_bytes()
    )
    subprocess.run(
        ["g++", "--std=c++17", str(SOURCE), "--output", "solution"], check=True
    )
    if call("submit", str(SOURCE), "--arg", "role=" + peer)["kind"] != "refused":
        raise RuntimeError("submit accepted a role override")
    if role == "alice":
        if omit != "all":
            select(SOLVER / "main.cpp", False)
        graph = {"vertices": 4, "edges": [[0, 1, 2], [0, 1, 2]]}
        graph["cores"] = solve(graph["vertices"], graph["edges"])
        Path("message").write_text(json.dumps(graph))
        if (
            call("post", "message", "--arg", "from=" + peer, "--wait", "15")["kind"]
            != "refused"
        ):
            raise RuntimeError("board accepted a forged sender")
        receipt = board("post", "message")
    else:
        until = time.monotonic() + 40
        while time.monotonic() < until:
            page = board("read")
            if page["messages"]:
                break
            time.sleep(0.25)
        else:
            raise TimeoutError("Alice's graph")
        (first,) = page["messages"]
        graph = json.loads(first["text"])
        cores = solve(graph["vertices"], graph["edges"])
        if first["from"] != peer or cores != graph["cores"]:
            raise ValueError(first)
        Path("message").write_text(json.dumps({"checked": first["id"], "cores": cores}))
        receipt = board("send", "message", "--arg", "to=" + peer)
    if "id" not in receipt:
        raise RuntimeError(receipt)
    if omit not in {role, "all"}:
        select(SOURCE, True)


if __name__ == "__main__":
    main(*sys.argv[1:])
