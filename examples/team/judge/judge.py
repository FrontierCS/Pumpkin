"""Prepare C++ test cases and grade the output returned by the compiler runner."""

import hashlib
import json
import re
import sys
from pathlib import Path

from hypergraph.judge import core_numbers

FIXTURE = Path("/opt/hypergraph/graph.json")


def graphs():
    return [
        {"vertices": 4, "edges": [[0, 1, 2], [0, 1, 2]]},
        json.loads(FIXTURE.read_bytes()),
    ]


def graph_input(graph):
    edges = graph["edges"]
    lines = [f"{graph['vertices']} {len(edges)}"]
    lines.extend(" ".join(map(str, [len(edge), *edge])) for edge in edges)
    return "\n".join(lines) + "\n"


def prepare(path):
    source = path.read_bytes()
    digest = "sha256:" + hashlib.sha256(source).hexdigest()
    try:
        source.decode("utf-8")
    except UnicodeDecodeError:
        return {"digest": digest, "inputs": None}
    return {"digest": digest, "inputs": [graph_input(graph) for graph in graphs()]}


def grade(outputs):
    cases = graphs()
    if outputs is None or len(outputs) != len(cases):
        return {"status": "invalid", "score": 0}
    points = 0
    for output, graph in zip(outputs, cases):
        if output is None or len(output.encode()) > 1024:
            return {"status": "invalid", "score": 0}
        tokens = output.split()
        if len(tokens) != graph["vertices"] or any(
            not re.fullmatch(r"[+-]?[0-9]+", token) for token in tokens
        ):
            return {"status": "invalid", "score": 0}
        points += list(map(int, tokens)) == core_numbers(graph)
    return {"status": "scored" if points == len(cases) else "wrong", "score": points}


if __name__ == "__main__":
    if sys.argv[1] == "prepare":
        print(json.dumps(prepare(Path(sys.argv[2]))))
    elif sys.argv[1] == "grade":
        print(json.dumps(grade(json.loads(sys.argv[2]))))
    else:
        rows = json.loads(sys.argv[3])
        points = (row["score"] for row in rows if row["score"] is not None)
        Path(sys.argv[2]).write_text(
            json.dumps({"score": max(points, default=None), "roles": rows})
        )
