"""Alice tests her degree baseline. Bob checks her graph with a peeling implementation."""

import json
import subprocess
import sys
import time
from pathlib import Path

from client import board, ctl

SOLVER = Path("/opt/team/solver")
SOURCE = Path("solution.cpp")


def select(role, path, finalize):
    reply = ctl(
        "call",
        "select_" + role,
        str(path),
        "--arg",
        f"finalize={str(finalize).lower()}",
    )
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
    if ctl("call", "select_" + peer, str(SOURCE))["kind"] != "refused":
        raise RuntimeError("peer's selection grant was accepted")
    if role == "alice":
        if "error" not in board(op="post", text="test", **{"from": peer}):
            raise RuntimeError("board accepted a forged sender")
        if omit != "all":
            select(role, SOLVER / "main.cpp", False)
        graph = {"vertices": 4, "edges": [[0, 1, 2], [0, 1, 2]]}
        graph["cores"] = solve(graph["vertices"], graph["edges"])
        receipt = board(op="post", text=json.dumps(graph))
    else:
        until = time.monotonic() + 40
        while time.monotonic() < until:
            page = board(op="read", stream="board", after=0)
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
        receipt = board(
            op="send",
            to=peer,
            text=json.dumps({"checked": first["id"], "cores": cores}),
        )
    if "id" not in receipt:
        raise RuntimeError(receipt)
    if omit not in {role, "all"}:
        select(role, SOURCE, True)


if __name__ == "__main__":
    main(*sys.argv[1:])
