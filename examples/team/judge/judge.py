"""Compile each source in its own runner and compare its output with an exhaustive oracle."""

import hashlib
import json
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# The judge image includes the single-agent example's graph and independent subset oracle.
from hypergraph.judge import core_numbers

FIXTURE = Path("/opt/hypergraph/graph.json")
CTL = "/.pumpkin/bin/sandboxctl"


def ctl(op, *args):
    done = subprocess.run(
        [CTL, "call", "cpp." + op, *args], capture_output=True, text=True, check=True
    )
    return json.loads(done.stdout)


def wait(lease, wanted):
    while True:
        status = ctl("status", lease)
        if status["state"] == wanted:
            return status["last"]
        if status["state"] not in {"starting", "running", "closing"}:
            raise RuntimeError(status)
        time.sleep(0.1)


def run(lease, seconds, *argv):
    ctl("run", lease, "--timeout", str(seconds), "--", *argv)
    result = wait(lease, "ready")
    if result["end"] == "unstarted":
        raise RuntimeError(result)
    return result["stdout"] if result["end"] == {"exited": {"code": 0}} else None


def score_source(raw):
    try:
        raw.decode("utf-8")
    except UnicodeDecodeError:
        return "invalid", 0
    graphs = [
        {"vertices": 4, "edges": [[0, 1, 2], [0, 1, 2]]},
        json.loads(FIXTURE.read_bytes()),
    ]
    lease = str(ctl("open")["lease"])
    try:
        wait(lease, "ready")
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "solution.cpp").write_bytes(raw)
            ctl("put", lease, directory, "/pumpkin/home/src")
            copied = wait(lease, "ready")
            if "moved" not in copied["end"]:
                raise RuntimeError(copied)
        if (
            run(
                lease,
                20,
                "sh",
                "-c",
                "sha256sum src/solution.cpp && exec g++ --std=c++17 src/solution.cpp --output solution",
            )
            is None
        ):
            return "invalid", 0
        points = 0
        for graph in graphs:
            edges = graph["edges"]
            lines = [
                f"{graph['vertices']} {len(edges)}",
                *(" ".join(map(str, [len(edge), *edge])) for edge in edges),
            ]
            # Cap the answer below the session's stdout tail so the judge sees the whole file.
            command = 'ulimit -f 1; printf "%s" "$1" | ./solution > answer && head --bytes=1025 answer'
            output = run(
                lease, 3, "sh", "-c", command, "solve", "\n".join(lines) + "\n"
            )
            if output is None or len(output.encode()) > 1024:
                return "invalid", 0
            tokens = output.split()
            if len(tokens) != graph["vertices"] or any(
                not re.fullmatch(r"[+-]?[0-9]+", token) for token in tokens
            ):
                return "invalid", 0
            points += list(map(int, tokens)) == core_numbers(graph)
        return ("scored" if points == len(graphs) else "wrong"), points
    finally:
        ctl("close", lease)
        wait(lease, "closed")


def score(inputs):
    results = []
    for index in range(0, len(inputs), 2):
        role, path = inputs[index : index + 2]
        digest, status, points = None, "missing", None
        if path:
            source = Path(path).read_bytes()
            digest = "sha256:" + hashlib.sha256(source).hexdigest()
            status, points = score_source(source)
        results.append(
            {"role": role, "digest": digest, "status": status, "score": points}
        )
    return {
        "score": max(
            (r["score"] for r in results if r["score"] is not None), default=None
        ),
        "roles": results,
    }


if __name__ == "__main__":
    mode, result, *inputs = sys.argv[1:]
    if mode == "draft":
        (path,) = inputs
        status, points = score_source(Path(path).read_bytes())
        verdict = {"status": status, "score": points}
    else:
        verdict = score(inputs)
    Path(result).write_text(json.dumps(verdict))
