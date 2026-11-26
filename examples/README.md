# Pumpkin examples

| Workflow | What it shows |
| --- | --- |
| [Hello](hello/task.star) | A shell agent calls a job and submits a greeting for scoring. No model required. |
| [Codex](codex/task.star) | A Codex agent writes and submits a greeting. Requires a Responses API model that supports Codex's native tools. |
| [Hypergraph core numbers](hypergraph/README.md) | A Codex agent solves a graph problem and can ask the judge to score its work before submitting. Requires the same model capabilities as Codex. |
| [Team](team/README.md) | Two Codex agents write C++ hypergraph solvers, exchange a test graph and evaluate their source. A scripted version runs without a model. |
| [Native hypergraph benchmark](hypergraph-benchmark/README.md) | Five Codex agents develop C++ with a shared board, native training feedback and a timed final judge. Includes a model-free replay of their selections. |
| [Smoke](smoke/task.star) | A deployment check probes sandbox confinement in the agent, a runner lease and the judge. No model required. |
| [Assets](assets/task.star) | The final call leases a runner that holds pinned files read-only. It checks their SHA-256s, that each write there fails, and that a fresh lease sees them unchanged. No model required. It needs a provider whose classes hold assets, as Docker's and Modal's do. |
| [Companions](companions/task.star) | The final call leases a runner with as many companions as `--arg companions=<n>` says, on the needs that its args give. It checks each container's user, that they reach one another by name and no public address, and times their round trips. No model required. It needs a provider whose classes offer companions, as Modal's do. |

Run them in place, from the root of a Pumpkin checkout. `cargo xtask build` builds `pumpkin`
into the checkout's `.pumpkin/bin`, and the commands below run it from there. Runs use the
machine's [configuration](../docs/running.md#the-machines-configuration), and their records are
in `.pumpkin/runs`. For Codex, set `MODEL` to a model in your
[upstream configuration](../docs/models.md#upstreams-and-prices). The
[hypergraph guide](hypergraph/README.md#run-codex) and
[team guide](team/README.md#run-two-codex-agents) give their Codex run and verification commands.

```sh
cargo xtask build

.pumpkin/bin/pumpkin check examples/hello/task.star &&
.pumpkin/bin/pumpkin run examples/hello/task.star --wait

.pumpkin/bin/pumpkin check examples/codex/task.star --arg "model=${MODEL:?set MODEL}" &&
.pumpkin/bin/pumpkin run examples/codex/task.star --arg "model=$MODEL" --wait

.pumpkin/bin/pumpkin check examples/team/task.star &&
.pumpkin/bin/pumpkin run examples/team/task.star --wait

.pumpkin/bin/pumpkin check examples/smoke/task.star &&
.pumpkin/bin/pumpkin run examples/smoke/task.star --wait
```

Each example that runs a model takes `--arg budget_microusd=<n>`, a model budget for each agent in
millionths of a dollar, such as `500000` for $0.50. A budget needs the model's prices in the
configuration. Without the arg, the agents have no budget.

To check Team's cross-agent message, selected C++ source, scores and cleanup in a run's record,
run `python3 examples/team/verify.py .pumpkin/runs/<id>`. To exercise missing submissions, pass
`--arg omit=bob` or `--arg omit=all` to the run and the matching `--omit bob` or `--omit all`
to the checker.

To verify a controller and a provider with Smoke, run `cargo xtask live ec2` or
`cargo xtask live modal` in a Pumpkin checkout, as [development testing](../docs/dev/testing.md)
says.
