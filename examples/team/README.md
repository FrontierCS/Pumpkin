# Two C++ solvers for hypergraph core numbers

Alice and Bob submit C++17 programs that compute the core number of every vertex in a
hypergraph. A vertex has core number at least `k` if it belongs to a vertex set in which
every vertex has at least `k` incident edges wholly inside the set. Removing a vertex
removes every edge that touches it. Repeated edges count separately.

Each program reads one graph on standard input. The first line gives the vertex and edge
counts. Each following line gives one edge's size and its distinct, zero-based vertices.
It prints one integer per vertex, in vertex order, separated by whitespace. For example:

```text
4 2
3 0 1 2
3 0 1 2
```

The output is `2 2 2 0`. Alice and Bob work in separate sandboxes, share one board and can
each finalize only their own C++ source. Alice publishes a test graph and its core vector.
Bob runs his program on her graph and sends Alice the message ID and his observed output.

The final body receives each role’s selected source or a missing value. It writes the selected
files into the judge sandbox. The judge records each source’s SHA-256 digest and opens a
fresh compiler sandbox for each program. It compiles the source and runs it on the graph above and the
[single-agent example's fixture](../hypergraph/graph.json). The judge checks each output
against an exhaustive enumeration of vertex subsets. One complete correct vector earns one
point. The team score is the greater individual score.

These small graphs check correctness. Compiler startup and sandbox overhead dominate the
work of solving them, so elapsed time does not enter the score.

## Run two Codex agents

Copy the whole `examples/` directory into a task repository with its Pumpkin launcher. Set
`MODEL` to a priced Responses API model that supports the Codex harness’s native tools.
Each role has a $0.25 model budget and a compiler. They share a fifteen-minute work deadline.
An admitted request can take a role past its budget. [Model budgets](../../docs/models.md#a-budget)

Alice writes a straightforward implementation and designs a small test of whole-edge removal.
Bob writes an incidence-list peeling algorithm, reads Alice’s case and sends her his result.
Each role saves a working source as soon as it compiles and can send two drafts to `eval`
after the exchange. The evaluator compiles and scores each draft in a fresh runner. Each agent
finalizes its best evaluated source with its own grant, even if the score is partial.

```sh
task=examples/team/codex.star
./pumpkin check "$task" --arg "model=${MODEL:?set MODEL}" &&
team=$(./pumpkin run "$task" --arg "model=$MODEL") &&
./pumpkin wait --outcome "$team" &&
python3 examples/team/verify.py ".pumpkin/runs/$team"
```

The checker requires a successful model response per role, a graph that Bob read and checked,
and a draft evaluation from each agent. It matches each selected digest to the judge’s copy and the
file given to the compiler, then scores each final runner’s recorded output against both expected
vectors. A compiling solver can earn
fewer than two points. Malformed output or a failed compilation makes a submission
invalid. Each source is available under the SHA-256 digest in the run’s `payloads/` directory.

## Run the scripted reference

Alice submits each vertex’s initial degree and earns one point. Initial degrees can drop when
a vertex and its incident edges are removed. Bob’s peeling algorithm earns both points. This
version uses the same board and final judge and runs without a model.

```sh
task=examples/team/task.star
./pumpkin check "$task" &&
team=$(./pumpkin run "$task") &&
./pumpkin wait --outcome "$team" &&
python3 examples/team/verify.py ".pumpkin/runs/$team"
```

To see Bob leave without selecting, give the run and its checker the same case.

```sh
missing=$(./pumpkin run examples/team/task.star --arg omit=bob) &&
./pumpkin wait --outcome "$missing" &&
python3 examples/team/verify.py ".pumpkin/runs/$missing" --omit bob
```

Use `--arg omit=all` and `--omit all` for two missing selections. A missing role has a null
score, and the team score is null if both are missing. Invalid source, a failed execution,
or malformed output earns zero. A program returning well-formed incorrect vectors keeps its
points for the graphs it solved.

The checker reads each selected C++ file by its recorded digest. It checks Alice's post,
Bob's read and reply, the ordered scores, and one final call. It verifies the deletion
receipts for the agents, the board, the judge and each compiler sandbox.
