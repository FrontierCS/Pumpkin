# Bazel's Starlark test data

Mirrored from Bazel, at
https://github.com/bazelbuild/bazel/tree/a765dc177f48efb4c9e1b980609b838c9ba27f42/src/test/java/net/starlark/java/eval/testdata,
under its `LICENSE`, Apache-2.0. The files are unchanged. Left out, as they test no part of the
specification:
- `bench_*.star`, which hold benchmarks, not tests;
- `json.star`, which tests the `json` module: the file is one chunk, which the subset refuses
  whole as it parses, so `lang`'s own `json` test takes each of its cases that holds nothing the
  subset leaves out;
- `fields.star`, which tests the `struct` values that Bazel's test runner provides.
