# Hypergraph core numbers

Compute a core number for each of the 12 vertices in [graph.json](graph.json). A vertex belongs
to a *k*-core if it lies in a set of vertices where every member has degree at least *k*, counting
only hyperedges wholly inside the set. Its core number is the largest such *k*. Duplicate edges
each contribute one to the degree of every member. For example, four vertices sharing just one
hyperedge each have core number one. An isolated vertex has core number zero. The task takes
inspiration from [FrontierCS's hypergraph core numbers problem][problem].

From the task repository with Pumpkin installed, run the scripted CPU example on local Docker
using the [local configuration](../../README.md#run-it).

```sh
./pumpkin check examples/hypergraph/task.star
./pumpkin run examples/hypergraph/task.star --wait
```

The reference agent first sends the vertices' original degrees to `eval` and checks that the
job returns `score=0`. It then repeatedly removes a vertex of minimum degree and its incident
edges, and submits its core numbers with `final`. The final call returns `score=1` when the
entire JSON array matches, in vertex order. Each judge runs in a fresh sandbox and computes
the answer by enumerating vertex subsets.

To give the same graph to Codex, set `MODEL` to a Responses model with prices in your machine's
[upstream configuration](../../docs/models.md#upstreams-and-prices), then run

```sh
./pumpkin run examples/hypergraph/task.star --arg model="$MODEL" --wait
```

Codex receives the graph and submission instructions. Its work phase lasts up to five minutes,
and it can ask for two evaluations of up to 30 seconds each before submitting its final answer.
The model route stops admitting requests once reported usage reaches $0.50. An admitted request
can take the total past that amount. [Model budgets](../../docs/models.md#a-budget)

[problem]: https://github.com/FrontierCS/FrontierCS-2/blob/2fc5ff7c6e43e11c64d7d644b9808b010b3814b5/tasks/problems/HPC/hypergraph_core_numbers/repo/README.md#L9-L14
