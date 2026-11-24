# Hypergraph core numbers

Compute a core number for each of the 12 vertices in [graph.json](graph.json). A vertex belongs
to a *k*-core if it lies in a set of vertices where every member has degree at least *k*, counting
only hyperedges wholly inside the set. Its core number is the largest such *k*. Duplicate edges
each contribute one to the degree of every member. For example, four vertices sharing just one
hyperedge each have core number one. An isolated vertex has core number zero.

## Run Codex

From the root of a Pumpkin checkout, with `pumpkin` built as the [examples guide](../README.md)
says, set `MODEL` to a Responses model in your machine's
[upstream configuration](../../docs/models.md#upstreams-and-prices), then run

```sh
task=examples/hypergraph/task.star
.pumpkin/bin/pumpkin check "$task" --arg "model=${MODEL:?set MODEL to the configured model name}" &&
codex=$(.pumpkin/bin/pumpkin run "$task" --arg "model=$MODEL") &&
.pumpkin/bin/pumpkin wait "$codex" &&
python3 examples/hypergraph/verify.py ".pumpkin/runs/$codex"
```

Codex receives the graph and submission instructions. Its work phase lasts up to five minutes,
and it can ask for two evaluations of up to 30 seconds each before submitting its final answer.
Add `--arg budget_microusd=500000` to `check` and `run` to stop the model route admitting
requests once reported usage reaches $0.50, which needs the model's prices. An admitted request
can take the total past that amount. [Model budgets](../../docs/models.md#a-budget)

Each judge runs in a fresh sandbox and enumerates vertex subsets. It awards one point for the
exact vector and zero otherwise. The checker requires a successful model response with output
tokens, a finalized vector of 12 integers, the matching score, and a recorded deletion for every
sandbox. Compare the reported commit and sandbox providers with the build and provider you
intended to run.
