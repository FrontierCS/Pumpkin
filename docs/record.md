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
| the workflow as it ran, where each role was placed, and each provider's report, whose strings name the provider's account | `ir.json`, `provider.json` |
| the text of each file the evaluation read, from which each body is evaluated | `sources/task/`, `sources/library/` |
| what the driver said after the start, such as why a sandbox failed to start | `driver.log` |
| the agent's output, and what it left in its `out/` | `agent/log/`, `agent/out/` |
| each call's target, a job function's or the final call's: each command's output, each file its body got, and what the target held in `/pumpkin/out` as the call ended | `jobs/<n>/log/<c>/`, `jobs/<n>/got/<k>/`, `jobs/<n>/out/` |
| each lease's runner sandbox: its output and what it wrote | `leases/<n>/log/`, `leases/<n>/out/` |
| each call's payload, the selection among them | `payloads/<digest>`, by the SHA-256 that the log and `outcome.json` name |
| each model request and response, as they went through the route | `model/` |
| the arguments that started the run, and the Pumpkin that ran it | `record.json` |

The `runtime` crate's `record` module says each file's fields. `ir.json` and `provider.json` are
written before the log and never change, the log gains a whole line at a time, and the driver
writes `outcome.json` last. A run is live while its driver holds the lock on `driver.pid`; a run
whose driver ended without an `outcome.json` is lost. A reap, once the driver has ended, adds a
line to `reaps.jsonl` for each sandbox it deleted, removes `sockets/`, and writes a lost run's
`outcome.json`, as failed.

`outcome.json`'s `state` is `finished`, `failed`, with a `reason`, or `interrupted`, with the
`signal` that stopped it. A finished run's `work_end` says how the agent's work ended, and its
`verdict` what came of the selection: `returned`, with the `submission`, its payload's digest and
size, and the final call's `result`, the `values` the run shows, in the order its body returned
them; `no_result`, with the `submission` and the `failure` of the final call, a `timeout` or an
`error` with its `reason`; or `no_submission`. `pumpkin wait --outcome <run>` prints the file on its
standard output once the run has ended, and says the summary on its standard error:

```console
$ ./pumpkin wait --outcome 234cd166 2> /dev/null
{
  "state": "finished",
  "usage": {
    "model_requests": 0,
    "ran_ns": "2289300505",
    "sandboxes": [
      {
        "class": "container",
        "count": 3,
        "lasted_ns": "3158040199",
        "provider": "local"
      }
    ],
    "tokens": {
      "input": 0,
      "output": 0,
      "responses": 0
    }
  },
  "verdict": {
    "kind": "returned",
    "result": {
      "values": [
        {
          "name": "score",
          "value": 6
        },
        {
          "name": "lines",
          "value": 1
        }
      ]
    },
    "submission": {
      "finalized": true,
      "payload": {
        "digest": "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03",
        "size": 6
      }
    }
  },
  "work_end": {
    "kind": "agent_exited",
    "status": {
      "code": 0
    }
  }
}
```

`usage` holds the run's duration, the sandboxes counted by provider and class with their lifetimes,
both in nanoseconds written as strings, and the model requests and tokens.

[Workflows](workflows.md) says what the log keeps of each call.

## Model exchanges

`model/` keeps every exchange as the route saw it pass: each request as it went upstream, or as
the client sent it if the route refused it, and each response as the upstream sent it. A part
that every request of a conversation repeats, such as its list of tools or an earlier message, is
kept once, by its digest, in `model/items/`. No key is in the record: the route keeps the names of
the headers it left behind, never their values.

A translated exchange names its upstream API in `kept.upstream_api` on its `model_answered`
line. Read the archived bodies as that API. The line's `api` and `model` identify the caller's
route for accounting. With no translation, the caller and upstream speak the same API.

## The page

`pumpkin view` serves the runs of a runs directory, or a controller's, to this machine's browser,
and prints the address to open. The page lists the runs, newest first, each with what `ls` says of
it, and for a run shows its summary, a lane for each sandbox over time, who talks to whom, and its
log, which it follows while the run is live. Each caller that used a model has a trace: its requests
as threads of steps, each with what it added and the model's answer.

The page listens on 127.0.0.1 alone and asks for the token in its address, so neither another
machine, another user of this one nor another site's page can read the runs. Its only change to a
run is a cancel, which runs `pumpkin cancel`.

`pumpkin view --export <dir>` writes the runs' data as files that a static copy of the page reads,
with a stand-in for each sandbox's id, the machine's home and user, and each string of a provider's
report, as `pumpkin view --help` says; a home, a user or a report's string shorter than 8 bytes is
kept as it is. A controller deployed with a public page publishes its runs this way as their jobs
run, to a page that a team reads at one address; [`deploy/aws`](../deploy/aws/README.md) says how to
set it up and take a run down.

## Spend

A record keeps what a run spent: each sandbox's class and its price an hour, as the provider's
settings give it, each model's prices for its tokens, as the configuration gave them when the run
started, and the tokens each response reported. `pumpkin ls` totals the cost; `wait`'s summary
gives it with the tokens and what each model budget spent. A cost with a `+` has parts without a
price, such as sandboxes on the machine's own engine.
