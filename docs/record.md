# Records and the page

Every run has a record: a directory of its own in the runs directory, `.pumpkin/runs/<id>/` by
default, which the run's driver writes as the run goes and which no sandbox can reach. `pumpkin
wait`, `pumpkin ls` and the page read the record wherever the run took place. This page says where
to look in a record, and what the page shows.

## What a record holds

| To see | Read |
| --- | --- |
| how the run ended, its result's values, its usage and estimated cost | `outcome.json` |
| every event of the run, in order, and what the driver did about it | `log.jsonl` |
| the workflow as it ran, where each role was placed, and each provider's report, whose strings name the provider's account | `ir.json`, `provider.json` |
| the text of each file the evaluation read, from which each body is evaluated | `sources/task/`, `sources/library/` |
| what the driver said after the start, such as why a sandbox failed to start | `driver.log` |
| each agent's output, and what it left in its `out/` | `agents/<role>/log/`, `agents/<role>/out/` |
| each call's target, a job function's or the final call's: each command's output, each file its body got, and what the target held in `/pumpkin/out` as the call ended | `jobs/<n>/log/<c>/`, `jobs/<n>/got/<k>/`, `jobs/<n>/out/` |
| a work-lifetime target's log, output directory and physical sandbox | `shared/<role>/log/`, `shared/<role>/out/`, `shared/<role>/sandbox.json` |
| each lease's runner sandbox: its output and what it wrote | `leases/<n>/log/`, `leases/<n>/out/` |
| each call's payload, including each agent's submission | `payloads/<digest>`, by the SHA-256 that the log and `outcome.json` name |
| each model request and response, as they went through the route | `model/` |
| the arguments that started the run, and the Pumpkin that ran it | `record.json` |

The `runtime` crate's `record` module says each file's fields. `ir.json` and `provider.json` are
written before the log and never change, the log gains a whole line at a time, and the driver
writes `outcome.json` last. A run is live while its driver holds the lock on `driver.pid`; a run
whose driver ended without an `outcome.json` is lost. A reap, once the driver has ended, adds a
line to `reaps.jsonl` for each sandbox it deleted, removes `sockets/`, and writes a lost run's
`outcome.json`, as failed.

A finished `outcome.json` has `state: "finished"`. Its `agents` array lists every agent in
declaration order. Each entry names the role, the reason its work ended, and its submission.
A submission gives the submitted payload's digest and byte size and whether the agent finalized
it. A role that submitted nothing has a JSON `null` submission. An empty file has a digest
and a size of zero.

The `verdict` records the final call. If its `kind` is `returned`, the result holds the values
in the order the final body returned them. If it is `no_result`, `failure` records a timeout
or an error and its reason. A failed run instead records `state: "failed"` and a reason.
An interrupted run records `state: "interrupted"` and the signal.

For example, to see who submitted and what the judge returned for `examples/hello`:

```console
$ ./pumpkin wait --outcome 234cd166 2> /dev/null | jq '{agents: [.agents[] | {role, bytes: .submission.payload.size}], verdict}'
{
  "agents": [
    {"role": "agent", "bytes": 6}
  ],
  "verdict": {
    "kind": "returned",
    "result": {
      "values": [
        {"name": "score", "value": 6},
        {"name": "lines", "value": 1}
      ]
    }
  }
}
```

`usage` holds the run's duration, the sandboxes counted by provider and class with their lifetimes,
both in nanoseconds written as strings, and the model requests and tokens.

The log identifies agent callers by their role index (`agent-<n>`). Jobs and leases have
run-wide numbers. Calls on a shared sandbox keep their command output and the files their
bodies read under `jobs/<n>/`. Usage counts the shared sandbox once across all its calls. If a sweep deletes an
allocation whose role is unknown, `usage.unplaced_sandbox` marks the cost as partial. The
allocation contributes no estimate to a class’s count, time or price.
The `start_final_call` action lists the selection for each agent by role.
[Workflows](workflows.md) describes what the log keeps of each call.


## Model exchanges

`model/` keeps every exchange as the route saw it pass: each request as it went upstream, or as
the client sent it if the route refused it, and each response as the upstream sent it. A part
that every request of a conversation repeats, such as its list of tools or an earlier message, is
kept once, by its digest, in `model/items/`. No key is in the record: the route keeps the names of
the headers it left behind, never their values.

## The page

`pumpkin view` serves the runs of a runs directory, or a controller's, to this machine's browser,
and prints the address to open. The page lists the runs, newest first. Open a run for its summary,
a lane for each sandbox over time, who talks to whom, and its log, which the page follows while the
run is live. Each caller that used a model has a trace of its requests as threads of steps, each
with what it added and the model's answer.

The page listens on 127.0.0.1 alone and asks for the token in its address, so neither another
machine, another user of this one nor another site's page can read the runs. Its only change to a
run is a cancel, which runs `pumpkin cancel`.

`pumpkin view --export <dir>` writes the runs' data as files that a static copy of the page reads,
with a stand-in for each sandbox's id, the machine's home and user, and each string of a provider's
report, as `pumpkin view --help` says; a home, a user or a report's string shorter than 8 bytes is
kept as it is. A controller deployed with a public page publishes its runs this way as their jobs
run, to a page that a team reads at one address; [`deploy/aws`](../deploy/aws/README.md) says how to
set it up and take a run down.

## Estimated cost

Pumpkin estimates the cost of reported model tokens and worker sandbox time at the rates recorded
for the run. Controller time, image builds, storage, network and provider billing adjustments are
outside the estimate.

- `./pumpkin ls RUN` shows the estimate so far to the nearest cent. A trailing `+` means some
  tokens or worker time within that scope are unpriced, or a response left usage unreported.
  A dash means there is no priced amount.
- `./pumpkin wait RUN` shows the token and worker subtotals to the nearest millionth of a dollar.
  The summary names unpriced models and shows the time and hourly rate, when recorded, for each
  worker class. The summary and the run page show an unavailable estimate when nothing is priced.
- `./pumpkin wait --outcome RUN | jq '.usage.usd'` reads the recorded estimate in dollars.
  An absent `usd` field means the run recorded no priced amount. In `.usage.sandboxes[]`, a
  class without `usd_per_hour` contributed time but no dollars.

The viewer's list takes a finished run's estimate from its outcome when that outcome records
usage. Older outcomes can omit a model's entire cost if a token category lacked a rate. `ls`,
`wait` and the detailed run page replay the log and include the priced categories.

Model input is split into regular input, reported cache reads and reported cache writes. Input
without a cache breakdown takes the regular input rate. Reasoning tokens are part of output.
If cache reads and writes together exceed the reported input for a model on an API, Pumpkin
omits that model's input and output on that API from the estimate. With a valid breakdown,
a missing rate omits only that category. The viewer's model trace sums only steps whose token
cost it can price in full.

A worker's `usd_per_hour` covers the whole sandbox, including all its GPUs. Pumpkin multiplies
that rate by `lasted_ns` in hours, from the request for the sandbox to its recorded end, or the
last log entry while it is live. A lease's `charged_ns` measures time against its lease budget.
Provider invoices can differ even when all usage is priced. For example, EC2 bills by instance
state and applies a minimum on start, and Modal accounts for CPU and memory bursts.
[EC2 instance billing](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-instance-lifecycle.html)
and [Modal sandbox pricing](https://modal.com/docs/guide/sandbox-resources) describe those rules.
