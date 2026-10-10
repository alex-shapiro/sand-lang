# Example benchmarks: Sand vs CPython

Every program in `examples/` has an idiomatic CPython port in `python/`.
`run.py` runs both on scaled-up workloads, checks that their output is byte-identical,
and prints a timing table, to find where Sand is slower than CPython 3.14.

## Usage

```
cargo build --release                       # run.py does not build anything
python3.14 bench/examples/run.py            # all workloads, 5 timed runs each
python3.14 bench/examples/run.py --only life,search_index --runs 10
```

Options:

| option          | meaning                                                                |
| --------------- | ---------------------------------------------------------------------- |
| `--only NAMES`  | comma-separated workload names or prefixes (`search_index` = all four) |
| `--runs N`      | timed runs per implementation (default 5)                              |
| `--sand PATH`   | Sand binary (default `target/release/sand`)                            |
| `--python PATH` | interpreter for the ports (default: the 3.14 running `run.py`)        |
| `--any-python`  | allow a version other than 3.14 (ratios are then not comparable)       |
| `--keep`        | keep the generated inputs (the temp dir is printed)                    |

For each workload, `run.py` generates its inputs into a temporary directory,
does one untimed warm-up run of each implementation, then N timed runs,
alternating Sand and Python. Every run's output (and any files it writes)
must match the other implementation's; otherwise the script prints the first
differing line, saves both outputs next to the inputs, and exits with status 1.

Times are wall-clock times of whole processes, including start-up and
compilation. Sand runs with `--max-memory=8G` and exactly the permissions
each program needs.

## Files

| file                        | contents                                                      |
| --------------------------- | ------------------------------------------------------------- |
| `run.py`                    | harness and the workload definitions                          |
| `gen.py`                    | deterministic input generators                                |
| `servers.py`                | local HTTP page server, fake S3 server and chat client driver |
| `python/*.py`, `python/s3/` | CPython ports, one per example                                |

Ports use the same algorithms and data structures as their Sand counterparts,
and reproduce Sand's output where its semantics differ (float formatting
`1e20` / `NaN`, IEEE results in `calc`, JSON numbers as floats in `template`).
Notable translation choices:

- `Result`/`?` error handling becomes exceptions.
- `ledger`'s `Monoid`/`Render` traits, which Sand implements for structs and
  builtin types (`int`, `str`, tuples, lists), become
  `functools.singledispatch` functions; the identity element that Sand passes
  implicitly via the trait dictionary is passed explicitly.
- `http_fetch` uses `urllib.request` on a thread pool (one thread per URL, as
  Sand spawns one task per URL); `chat_server` uses asyncio streams; `s3` uses
  `http.client`, `hashlib` and `hmac`.
- Python's `re` module caches compiled patterns, so the ports' per-call
  `re.sub(pattern, ...)` / `re.findall(pattern, ...)` mirror the Sand code's
  per-call `regex.new(...)` but do not recompile.

## Profiling a workload

The `profiling` profile is the release profile with `debug = 1`, so macOS
`sample` (or Instruments) shows Rust function names and line numbers:

```
cargo build --profile profiling
python3.14 bench/examples/gen.py /tmp/sand-inputs
target/profiling/sand run examples/adventure.sand --allow-stdin < /tmp/sand-inputs/adventure.txt > /dev/null &
sample $! 3 1 -mayDie -file /tmp/adventure.sample.txt
```

Make the input bigger than the benchmark's (e.g. `gen.adventure_script(60_000)`)
so the run outlasts the sampling window. Natives show up as
`stdlib::core::register_*::{closure}`; `atos -i -o target/profiling/sand -l
LOAD_ADDRESS ADDR` maps a sampled address to its source line.

## Results

Apple M-series laptop, `cargo build --release`, CPython 3.14.8, 5 runs each. Geometric mean is 0.59.

| workload            | Sand   | CPython | ratio |
| ------------------- | ------ | ------- | ----- |
| life                | 231 ms | 400 ms  | 0.58  |
| life_big            | 232 ms | 423 ms  | 0.55  |
| calc                | 218 ms | 353 ms  | 0.62  |
| adventure           | 160 ms | 166 ms  | 0.96  |
| template_render     | 208 ms | 298 ms  | 0.70  |
| template_compile    | 142 ms | 189 ms  | 0.75  |
| ledger              | 274 ms | 554 ms  | 0.49  |
| search_index_index  | 191 ms | 196 ms  | 0.98  |
| search_index_search | 199 ms | 609 ms  | 0.33  |
| search_index_add    | 137 ms | 241 ms  | 0.57  |
| search_index_stats  | 134 ms | 359 ms  | 0.37  |
| http_fetch          | 88 ms  | 211 ms  | 0.42  |
| chat_server         | 484 ms | 542 ms  | 0.89  |
| s3_put              | 61 ms  | 81 ms   | 0.75  |
| s3_get              | 136 ms | 241 ms  | 0.56  |
| s3_ls               | 79 ms  | 82 ms   | 0.96  |
| s3_small_ops        | 255 ms | 983 ms  | 0.26  |
