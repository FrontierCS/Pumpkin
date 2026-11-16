# starlark-go's test data

Mirrored from starlark-go, at commit `4f4267fb140d1e8b91c9bce55cee2f809e8840dd`, under its
`LICENSE`, BSD-3-Clause:
- `starlark/`, from https://github.com/google/starlark-go/tree/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/starlark/testdata;
- `resolve/`, from https://github.com/google/starlark-go/tree/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/resolve/testdata;
- `syntax/`, from https://github.com/google/starlark-go/tree/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/syntax/testdata.

The files are unchanged. Left out, as they test no part of the specification:
- `starlark/benchmark.star`, which holds benchmarks, not tests;
- `starlark/paths.star`, a library the tests don't load;
- `starlark/math.star`, `time.star`, `proto.star` and `module.star`, and the `proto/` data,
  which test modules of starlark-go's own;
- `starlark/json.star`, which tests the `json` module: the file is one chunk, which the subset
  refuses whole as it parses, so `lang`'s own `json` test takes each of its cases that holds
  nothing the subset leaves out;
- `syntax/scan.star`, the input of the scanner's and the parser's own tests.
