"""Try the original degrees with the evaluator, then peel the graph for the final submission."""

import json
import subprocess
import time
from pathlib import Path


def degrees(vertices, edges):
    counts = [0] * vertices
    for edge in edges:
        for vertex in edge:
            counts[vertex] += 1
    return counts


def peel(vertices, edges):
    remaining = set(range(vertices))
    cores = [0] * vertices
    level = 0
    while remaining:
        counts = degrees(vertices, edges)
        vertex = min(remaining, key=counts.__getitem__)
        # Later removals at this level can have lower degree after an incident edge disappears.
        level = max(level, counts[vertex])
        cores[vertex] = level
        remaining.remove(vertex)
        edges = [edge for edge in edges if vertex not in edge]
    return cores


def ctl(*args):
    reply = subprocess.check_output(["/.pumpkin/bin/sandboxctl", *args], text=True)
    return json.loads(reply)


def main():
    graph = json.loads(Path(__file__).with_name("graph.json").read_bytes())
    vertices, edges = graph["vertices"], graph["edges"]
    answer = Path("cores.json")
    answer.write_text(json.dumps(degrees(vertices, edges)) + "\n")
    job = ctl("call", "eval", str(answer))["job"]
    status = ctl("status", str(job))
    while status["state"] in {"queued", "running"}:
        time.sleep(1)
        status = ctl("status", str(job))
    if status["state"] != "completed" or status["returned"] != {"score": 0}:
        raise RuntimeError(
            f"expected the evaluator to reject raw degrees, got {status}"
        )
    answer.write_text(json.dumps(peel(vertices, edges)) + "\n")
    ctl("call", "final", str(answer), "--arg", "finalize=true")


if __name__ == "__main__":
    main()
