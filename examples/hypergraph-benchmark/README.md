# Native hypergraph benchmark

Five Codex agents implement exact hypergraph core numbers in a native C++ starter repository.
Alice, Bob, Carol, Dan and Eve each have eight vCPUs, 32 GiB and a private Git checkout. They
share a 90-minute deadline and a message board. Each can send private
messages, train on two feedback graphs, select a patch archive and finish independently.
The [small team example](../team/README.md) has a scripted workload suitable for a quick run.

## Run the team

Run these commands from the root of a Pumpkin checkout, with `pumpkin` built as the
[examples guide](../README.md) says. Set `MODEL` to a Responses API model in your
[upstream configuration](../../docs/models.md#upstreams-and-prices) that supports Codex's native
tools. The five agents work at once, each in a sandbox of eight CPUs and 32 GiB, so the run needs
providers that fit them all, such as EC2's.

```sh
task=examples/hypergraph-benchmark/task.star
.pumpkin/bin/pumpkin check "$task" --arg "model=${MODEL:?set MODEL}" &&
team=$(.pumpkin/bin/pumpkin run "$task" --arg "model=$MODEL") &&
.pumpkin/bin/pumpkin wait --outcome "$team" &&
python3 examples/hypergraph-benchmark/verify.py ".pumpkin/runs/$team"
```

To give each agent a model budget, add `--arg budget_microusd=5000000` for $5, which needs prices
for the model. Compute is billed separately.

The task statement, the starter's stub, the edit scope, the evaluator and the verifier are
adapted from a public benchmark under [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/).
The image builds fetch [Ligra](https://github.com/jshun/ligra/tree/8763202ef09ebf8c5f7d7787cf72f3ca55efbe4c),
Julian Shun's MIT-licensed framework, at a pinned commit. Its tree is the starter's framework,
its `HyperKCore-Efficient.C` is the judge's reference, and its `randHypergraph` makes the four
graphs, which the judge's build checks by their SHA-256 digests.

## Check the final result

The final judge compiles each present selection and evaluates roles serially. It checks the
complete core vector and times seven executions on each of four graphs at eight threads. The
score is `100 * exp(-t / 0.25)`, where `t` is the median of the four per-graph median candidate
times in seconds. The verifier also measures at one thread. A candidate can earn a timing score
despite failing a one-thread workload. Such a candidate cannot earn the separate reference win,
which requires being at least 3% faster on every workload at both thread counts.

The team receives the highest individual score and all tied winners. A missing selection has
no score. A verifier error leaves the team result incomplete. The run record preserves the
selection digests, the native verifier's JSON for each evaluated role, and the native evidence.
With the `zstd` command available, `verify.py` checks the run's record: each selection's bytes,
the judge's verdict and timer samples for each role, the board messages and the sandbox deletion
receipts. It prints each role's score. Scores vary from run to run, even with the same model.

## Replay the selected archives

The replay reads the five selected submissions from the original record and checks their
digests and lengths. It writes them to `examples/benchmark-replay/`, in place of an earlier
replay's, which Git ignores and a run sends all the same. A fresh verifier compiles those
selections and repeats the measurements. Its verdict reports the original and fresh scores,
winners and whether it reproduced each outcome. Fresh timings can change the ranking. Replay
needs compute and makes no model calls.

```sh
python3 examples/hypergraph-benchmark/replay/prepare.py ".pumpkin/runs/${team:?set team to the run's id}" &&
.pumpkin/bin/pumpkin check examples/hypergraph-benchmark/replay/task.star &&
.pumpkin/bin/pumpkin run examples/hypergraph-benchmark/replay/task.star --wait --outcome
```
