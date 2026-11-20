"""Score a JSON vector against the induced subgraphs of the public hypergraph."""

import itertools
import json
import sys
from pathlib import Path


def core_numbers(graph):
    vertices = range(graph["vertices"])
    cores = [0] * len(vertices)
    for size in range(1, len(vertices) + 1):
        for subset in itertools.combinations(vertices, size):
            degrees = dict.fromkeys(subset, 0)
            for edge in graph["edges"]:
                if all(vertex in degrees for vertex in edge):
                    for vertex in edge:
                        degrees[vertex] += 1
            minimum = min(degrees.values())
            for vertex in subset:
                cores[vertex] = max(cores[vertex], minimum)
    return cores


def score(payload, expected):
    try:
        answer = json.loads(payload)
    except (ValueError, RecursionError):
        return 0
    return int(
        isinstance(answer, list)
        and all(type(v) is int for v in answer)
        and answer == expected
    )


if __name__ == "__main__":
    graph = json.loads(Path(__file__).with_name("graph.json").read_bytes())
    result = score(Path(sys.argv[1]).read_bytes(), core_numbers(graph))
    Path(sys.argv[2]).write_text(json.dumps({"score": result}) + "\n")
