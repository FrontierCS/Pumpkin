# Pumpkin examples

| Workflow | What it shows |
| --- | --- |
| [Hello](hello/task.star) | A shell agent calls a job and submits a greeting for scoring. No model required. |
| [Codex](codex/task.star) | A Codex agent writes and submits a greeting. Requires a Responses API model that supports Codex's native tools. |
| [Hypergraph core numbers](hypergraph/README.md) | A Codex agent solves a graph problem and can ask the judge to score its work before submitting. Requires the same model capabilities as Codex. |
| [Smoke](smoke/task.star) | A deployment check probes sandbox confinement in the agent, a runner lease and the judge. No model required. |

Copy the whole `examples/` directory next to `./pumpkin` and `.pumpkinversion` in your task
repository. Keep [`PUMPKIN.star`](PUMPKIN.star) and [`submit.star`](submit.star) in place so the
tasks can load their shared files. See [installing Pumpkin](../docs/running.md#installing) in the
Pumpkin source repository for launcher setup.

From the task repository, check and run the workflows below. For Codex, set `MODEL` to a model
in your [upstream configuration](../docs/models.md#upstreams-and-prices). The
[hypergraph guide](hypergraph/README.md#run-codex) gives its run and verification commands.

```sh
./pumpkin check examples/hello/task.star &&
./pumpkin run examples/hello/task.star --wait

./pumpkin check examples/codex/task.star --arg "model=${MODEL:?set MODEL}" &&
./pumpkin run examples/codex/task.star --arg "model=$MODEL" --wait

./pumpkin check examples/smoke/task.star &&
./pumpkin run examples/smoke/task.star --wait
```

For controller and provider verification with Smoke, see
[development testing](../docs/dev/testing.md).
