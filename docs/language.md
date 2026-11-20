# The workflow language

A workflow is written in a subset of [Starlark](https://github.com/bazelbuild/starlark/blob/master/spec.md),
the language of Bazel's files, with type annotations on a `def`'s parameters and return. This is
`examples/submit.star`, the body of a now function:

```python
"""The agent's submissions."""

def submit(run: Run, payload: Payload, finalize: bool = False) -> dict[str, str]:
    """Sets the calling agent's submission, replacing its previous payload.

    Args:
      run: the run's state.
      payload: the agent's submission.
      finalize: whether the call also locks the submission and ends the agent's work.

    Returns:
      `stored`, or `accepted` if the call locks the submission.
    """
    run.set_submission(payload)
    if finalize:
        run.end_work()
        return {"state": "accepted"}
    return {"state": "stored"}
```

The subset only takes away: a file it accepts, with its annotations dropped, is Starlark and means
what the specification says. Where it is narrower, it refuses the construct where it is written,
with what to write instead, rather than calling it unknown:

```text
lib.star:2:5: a `for` statement is not in Pumpkin's Starlark: build the list with a comprehension, as `[f(x) for x in xs]`
```

[Workflows](workflows.md) says what a workflow's values mean; this page says how files are
written and evaluated.

## Files and `load`

A task's files are below its root, the nearest directory at or above its entry file that holds a
`PUMPKIN.star`, which holds only comments. A file names another by a label: `//lib/bench.star`
below the task's root, or `@pumpkin//harness/codex/codex.star` below Pumpkin's standard library,
whose files load no task's.

`load` statements come first in a file. Each binds names that the loaded file's top level binds,
as they are or under another name:

```python
load("//hello/count.star", "count")
load("//lib/parts.star", "AGENT", judge_parts = "JUDGE")
```

A name that starts with `_` is private to its file. A file is evaluated once in an evaluation,
however many files load it, and its values never change after that.

The entry file is the one `pumpkin check` and `pumpkin run` are given. Only while its own
statements run can `workflow()`, `image()` and `package_name()` be called: at its top level, or in
a function it calls. It defines no function: functions, a body among them, live in the files it
loads.

## What the subset keeps

At a file's top level: `load`, assignments to a name or to names in a tuple or list, expression
statements, `pass`, and `def`. In a function's body: the same but `load` and `def`, and `return`
and `if`, `elif` and `else`. Parameters are positional, then keyword-only after a bare `*`, each
with a default or without.

Expressions: names, ints, floats, strings, lists, tuples, dicts, list and dict comprehensions,
calls, with keyword arguments and a `**` dict of them, methods called where they are named,
indexing, `a if c else b`, `or`, `and`, `not`, comparisons, which don't chain, `in`, `not in`,
`|` of two dicts, `+`, `-`, `*`, `/`, `//`, `%` and unary `-`.
A method call looks up the method on its receiver before evaluating its arguments. If the lookup
fails, the call ends there. [Starlark method calls](https://github.com/bazelbuild/starlark/blob/c0af0ea03dc90d3bbc021394b693e6957cc15030/spec.md#L2530-L2534)

Left out, each refused where it is written: the `for` statement, which a comprehension
replaces; `lambda`; a `def` inside a function; `*args`, `**kwargs` and `*` before an argument;
augmented assignment such as `+=`; slices; bitwise operators; `%` formatting of a string; a
sequence repeated by `*`; bytes; and, as in Starlark, a function that calls itself, even through
others.

## Values

`None`, bools, ints, floats, strings, lists, tuples and dicts, as in Starlark, with these limits:

- An int is 64 bits: an operation that overflows is refused, not widened.
- No value changes once made: a list or a dict has no method that would change it, such as
  `append` or `update`, so a new value is built by `+`, `|` or a comprehension.
- A dict's keys are strings or numbers.
- A string has no length or index, as Starlark's implementations count its elements differently.

The built-in functions are `bool`, `fail`, `float`, `int`, `len`, `sorted` and `str`; a string's
methods are `endswith`, `format`, `join`, `replace`, `split` and `startswith`; a dict's are `get`,
`items`, `keys` and `values`. Each takes fewer forms than Starlark's: `sorted` sorts numbers or
strings and takes no `key`, `split` takes a separator, `format` takes `{}`, `{0}` or `{name}`.
`json.encode` and `json.decode` convert between values and JSON text, as those of Bazel's `json`
module do.

## Types

A `def` may annotate each parameter after `:` and its return after `->`. A type is `None`,
`bool`, `int`, `float`, `str`, `list[T]`, `dict[K, V]`, whose `K` is `int`, `float` or `str`, or
one of the handles `Target`, `Runner`, `Run` and `Payload`. A `list` or a `dict` names its elements' types,
and a type is one type, with no `|`.

An annotation is checked as the function is called, against each argument, and as it returns,
against what it returns; a value that doesn't fit is refused there, and an unannotated parameter
takes any value. A variable takes no annotation.

```text
lib/verify.star:5:5: `_verified`'s `target` can't be the payload: give a `Target`
```

## Pumpkin's names

Every file can use these, besides Starlark's:

| Name | What it is |
| --- | --- |
| `workflow(*, name, roles, final, deadline_seconds)` | the workflow, which [Workflows](workflows.md) describes; called once |
| `arg(name, type, *, default)` | the value of `--arg <name>=<value>`, read as `type`, `str`, `int` or `bool`; without a `default`, it must be given |
| `image(recipe, *, context, mount, target, args, contexts)` | the image that the Dockerfile `recipe` builds; its value names it in a role |
| `package_name()` | the entry file's directory below the root, such as `tasks/bench` |
| `KiB`, `MiB`, `GiB` | 2^10, 2^20 and 2^30 |
| `USD` | 10^6, a US dollar in the millionths that a model budget counts |
| `MODEL_URL` | the URL of the model endpoint in a sandbox, `http://127.0.0.1:11235` |
| `MAX_MODEL_REQUESTS` | the most model requests a sandbox may have in flight, 7 |

`image()`'s recipe is a file of its context, the entry file's directory by default; `mount` adds
directories of the repository at paths in the context, `target` picks a stage, and `args` and
`contexts` give build arguments and named contexts. A recipe must pin what it fetches: an image by
its digest, a file by its checksum, a Git repository by its full commit; `pumpkin lock` writes the
fetch stage that pins a step's downloads.

## Handles

A body's first two parameters are handles, values that only the host makes, one per call. A
handle has methods and nothing else, and no JSON holds one, so a body can't return it.

| Handle | Methods |
| --- | --- |
| `Target`, a job's or the final call's target | `caller()`, `put(path, payload)`, `run(argv)`, `capture(argv, stdin = payload)`, `get(path)`, `with_runner(session, callback, ...)` |
| `Runner`, the lease passed to a callback | `put(tree, to)`, `run(argv, timeout_seconds = seconds)` |
| `Run`, the calling agent’s state | `set_submission(payload)`, `end_work()` |
| `Payload`, an opaque file supplied to the call | passed to `put`, to `capture` as stdin, or to `set_submission` |

[Workflows](workflows.md#bodies) says what each method does. A handle may be passed to a function
of the body's file, whose parameter it fits if annotated so.

## Bounds

Every evaluation is hermetic and ends. It reads files only through labels, below the task's root
and the standard library, and arguments only from `--arg`; it reads no clock, no environment and
no network, so the same files and arguments always give the same workflow. It evaluates at most
1,000,000 expressions, its values take at most 256 MiB, values and code nest at most 128 deep, and
at most 48 functions run at once; past any of these, it is refused at the expression that passed
it. A body's call is an evaluation of its own, bounded the same.

## Formatting and the lint

`pumpkin fmt` formats `.star` files in the one style the language's files are written in, and
`pumpkin check` lints the files it reads: names loaded or bound but never used, statements never
reached, a function that returns a value on some paths only, and a docstring for each file and
each public function, which names its parameters under `Args:` and says what it returns under
`Returns:`. `pumpkin check --help` lists every rule.
