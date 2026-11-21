"""Prepare C++ test cases and grade the output returned by the compiler runner."""

import hashlib
import json
import re
import sys
from pathlib import Path

from hypergraph.judge import core_numbers

FIXTURE = Path("/opt/hypergraph/graph.json")


def graphs():
    return {
        "repeated_edge": {"vertices": 4, "edges": [[0, 1, 2], [0, 1, 2]]},
        "fixture": json.loads(FIXTURE.read_bytes()),
    }


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
    return {
        "digest": digest,
        "inputs": {name: graph_input(graph) for name, graph in graphs().items()},
    }


def check(case, output):
    graph = graphs()[case]
    if len(output.encode()) > 1024:
        return None
    tokens = output.split()
    if len(tokens) != graph["vertices"] or any(
        not re.fullmatch(r"[+-]?[0-9]+", token) for token in tokens
    ):
        return None
    return int(list(map(int, tokens)) == core_numbers(graph))


if __name__ == "__main__":
    if sys.argv[1] == "prepare":
        print(json.dumps(prepare(Path(sys.argv[2]))))
    elif sys.argv[1] == "check":
        print(json.dumps(check(sys.argv[2], json.loads(sys.argv[3]))))
    else:
        rows = json.loads(sys.argv[3])
        points = (row["score"] for row in rows if row["score"] is not None)
        Path(sys.argv[2]).write_text(
            json.dumps({"score": max(points, default=None), "roles": rows})
        )
