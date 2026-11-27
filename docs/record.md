# Records and the page

Every run has a record: a directory of its own in the runs directory, `.pumpkin/runs/<id>/` by
default, which the run's driver writes as the run goes and which no sandbox can reach. `pumpkin
wait`, `pumpkin ls` and the page read only the record, so each says the same of a run, wherever
it ran. This page says where to look in a record, and what the page shows.

## What a record holds

| To see | Read |
| --- | --- |
| how the run ended, its result's values, its usage and cost | `outcome.json` |
| every event of the run, in order, and what the driver did about it | `log.jsonl` |
| when each sandbox's create began and returned, the phases of it that its provider timed, and when the sandbox met the gateway | `jq -c 'select(.event.kind == "sandbox_created" or .event.kind == "sandbox_met")' log.jsonl` |
| when each container met the gateway, in a sandbox that holds companions | `jq -c 'select(.event.kind == "container_met")' log.jsonl` |
| each publish of assets' files before the run started, one for each provider and digest whatever roles hold it, with whether it fetched the files and how long it took | `jq -c 'select(.event.kind == "asset_published")' log.jsonl` |
| the workflow as it ran, where each role was placed, and each provider's report, whose strings name the provider's account | `ir.json`, `provider.json` |
| the text of each file the evaluation read, from which each body is evaluated | `sources/task/`, `sources/library/` |
| what the driver said after the start, such as why a sandbox failed to start | `driver.log` |
| each agent's output, and what it left in its `out/` | `agents/<role>/log/`, `agents/<role>/out/` |
| each call's target, a job function's or the final call's: each command's output, each file its body got, and what the target held in `/pumpkin/out` as the call ended | `jobs/<n>/log/<c>/`, `jobs/<n>/got/<k>/`, `jobs/<n>/out/` |
| a `work_phase` target's log, output directory and physical sandbox | `shared/<role>/log/`, `shared/<role>/out/`, `shared/<role>/sandbox.json` |
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
A line of `pumpkin ls` shows a result's numbers, booleans and strings of one line, as far as the
line allows. `pumpkin wait` and the page's words for a run cut a long result short. They cut a
call's long reason for failing short too, as the page does each long string of a record.
`outcome.json` keeps the run's result whole, which `pumpkin wait --outcome` prints. The log keeps
whole what each call's body returned, or why it failed.

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

`driver` holds what the run's driver used, as it read itself as the run ended. `cpu_ns` and
`children_cpu_ns` give its CPU time and that of the children it waited for, as strings.
`peak_memory_bytes` gives the most memory it held at once. A controller's job adds `job`: its
systemd `unit`, and what the unit's cgroup counted, CPU time, peak memory and `peak_tasks_count`.
A figure that the driver couldn't read is `null`, and the record doesn't keep why. The
`outcome.json` that a reap writes of a lost run has no `driver`. `wait`'s summary says it on its
`driver:` line. The page says it in the same words, in its figure of the driver's CPU and that
figure's card.

The log identifies agent callers by their role index (`agent-<n>`). Jobs and leases have
run-wide numbers. Calls on a shared sandbox keep their command output and the files their
bodies read under `jobs/<n>/`. Usage counts the shared sandbox once across all its calls. If a sweep deletes an
allocation whose role is unknown, `usage.unplaced_sandbox` marks the cost as partial. The
allocation contributes no estimate to a class’s count, time or price.
The `start_final_call` action lists the selection for each agent by role.

For each call, the log keeps:

- the op and its reply, and the call's start
- each `put`, with its path and the payload's digest
- each `run`, with its argv and how it ended
- each `get`, with its path and the digest of what it got
- what the body returned, or why it failed


## Model exchanges

`model/` keeps every exchange as the route saw it pass: each request as it went upstream, or as
the client sent it if the route refused it, and each response as the upstream sent it. A part
that every request of a conversation repeats, such as its list of tools or an earlier message, is
kept once, by its digest, in `model/items/`. No key is in the record: the route keeps the names of
the headers it left behind, never their values.

## The page

`pumpkin view` serves the runs of a runs directory, or a controller's, to this machine's browser,
and prints the address to open. The page lists the runs, newest first, each with what `ls` says of
it, and for a run shows its summary, a lane for each sandbox over time, who talks to whom, and its
log, which it follows while the run is live. A sandbox's lane shows its start in parts, where the
log timed them. They are the wait for the provider's create, the create, the wait to meet the
gateway, and the setup. Hovering the start lists how long each took, and each phase of the create
that the provider timed. For a sandbox with companions, it also says how many containers met,
when the first and the slowest did, and which never met. Each caller that used a model has a
trace: its requests as threads of
steps, each with what it added and the model's answer. The page's colours follow the browser's light
or dark setting. The choice in its header overrides that, and the browser keeps it. `&scheme=light`,
`&scheme=dark` or `&scheme=system` after the token picks one in the address.

The page listens on 127.0.0.1 alone and asks for the token in its address, so neither another
machine, another user of this one nor another site's page can read the runs. It answers only a
request that names this machine as its host. Its only change to a run is a cancel, which runs
`pumpkin cancel --runs <dir> <run>`, or `pumpkin cancel <run>` for a controller's run, and shows
what that says.

## Spend

A record keeps what a run spent: each sandbox's class and its price an hour, as the provider's
settings give it, each model's prices for its tokens, as the configuration gave them when the run
started, and the tokens each response reported. `pumpkin ls` totals the cost; `wait`'s summary
gives it with the tokens and what each model budget spent. A cost with a `+` has parts without a
price, such as sandboxes on the machine's own engine.
