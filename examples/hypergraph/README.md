# Hypergraph core numbers

Compute a core number for each of the 12 vertices in [graph.json](graph.json). A vertex belongs
to a *k*-core if it lies in a set of vertices where every member has degree at least *k*, counting
only hyperedges wholly inside the set. Its core number is the largest such *k*. Duplicate edges
each contribute one to the degree of every member. For example, four vertices sharing just one
hyperedge each have core number one. An isolated vertex has core number zero.

## Run the reference agent

Copy `examples/` into a task repository and pin `./pumpkin` to the commit you want to verify.
When testing a release archive, use the examples and launcher from that release. The
[local Docker configuration](../../README.md#run-it) requires no model upstream.

```sh
task=examples/hypergraph/task.star
./pumpkin check "$task" &&
reference=$(./pumpkin run "$task") &&
./pumpkin wait "$reference" &&
python3 examples/hypergraph/verify.py reference ".pumpkin/runs/$reference"
```

The reference agent sends the original degrees to `eval` for a score of zero. It then repeatedly
removes a vertex of minimum degree and its incident edges, and submits the resulting core numbers
with `final` for a score of one. Each judge runs in a fresh sandbox and enumerates vertex subsets
to score the entire JSON array in vertex order.

## Run Codex

To give the same graph to Codex, set `MODEL` to a Responses model with prices in your machine's
[upstream configuration](../../docs/models.md#upstreams-and-prices), then run

```sh
task=examples/hypergraph/task.star
./pumpkin check "$task" --arg "model=${MODEL:?set MODEL to the configured model name}" &&
codex=$(./pumpkin run "$task" --arg "model=$MODEL") &&
./pumpkin wait "$codex" &&
python3 examples/hypergraph/verify.py codex ".pumpkin/runs/$codex"
```

Codex receives the graph and submission instructions. Its work phase lasts up to five minutes,
and it can ask for two evaluations of up to 30 seconds each before submitting its final answer.
The model route stops admitting requests once reported usage reaches $0.50. An admitted request
can take the total past that amount. [Model budgets](../../docs/models.md#a-budget)

The checker requires a successful model response with output tokens for Codex, and accepts a
valid wrong vector with a score of zero. For both agents it checks that the submission was
finalized, the score matches the vector, and every sandbox has a recorded deletion. Compare
the reported full commit and sandbox providers with the build and provider you intended to run.
