# Example benchmarks: Sand vs CPython

Every program in `examples/` has a faithful, idiomatic CPython port in
`python/`, and `run.py` runs both on scaled-up workloads, checks that their
output is byte-identical, and prints a timing table. The goal is to find where
the Sand interpreter (and its standard library) is slower than CPython 3.14,
and to measure progress towards parity.

## Usage

```
cargo build --release                       # run.py does not build anything
python3.14 bench/examples/run.py            # all workloads, 5 timed runs each
python3.14 bench/examples/run.py --only life,search_index --runs 10
```

Options:

| option          | meaning                                                                |
|-----------------|------------------------------------------------------------------------|
| `--only NAMES`  | comma-separated workload names or prefixes (`search_index` = all four) |
| `--runs N`      | timed runs per implementation (default 5)                              |
| `--sand PATH`   | Sand binary (default `target/release/sand`)                            |
| `--python PATH` | interpreter for the ports (default `/opt/homebrew/bin/python3.14`)     |
| `--keep`        | keep the generated inputs (the temp dir is printed)                    |

For each workload, `run.py` generates its inputs into a temporary directory,
does one untimed warm-up run of each implementation (which also primes the
page cache and Python's `__pycache__`), then N timed runs, alternating Sand and
Python. Every run's output (plus, where relevant, a hash of the files the
program wrote) must equal the other implementation's; any difference prints
the first differing line, saves both outputs next to the inputs, and makes the
script exit with status 1.

Times are wall-clock times of whole processes, so they include process
start-up, compilation (Sand) or bytecode loading (Python), and I/O. The table
shows the best and median of the N runs, `ratio` = Sand median / Python
median and `(best)` = Sand best / Python best. A ratio above 1 means Sand is
slower. The machine should be otherwise idle: background load makes the
medians noisy (the best-of-N ratio is the more robust number).

Sand programs are run as
`target/release/sand run FILE --max-memory=8G <sandbox flags> -- ARGS`, with
exactly the permissions they need (`--allow-stdin`, `--allow-read=DIR`,
`--allow-net=127.0.0.1:PORT`, ...). `sand run` resolves relative paths
against the script's directory, so all paths passed to the programs are
absolute.

## Files

| file                         | contents                                                              |
|------------------------------|-----------------------------------------------------------------------|
| `run.py`                     | the harness and the workload definitions                              |
| `gen.py`                     | deterministic (seeded) input generators; `python3.14 gen.py OUT_DIR` writes all inputs for manual runs and profiling |
| `servers.py`                 | local HTTP page server, fake S3 server and chat client driver         |
| `python/*.py`, `python/s3/`  | the CPython ports, one per example (`python/s3/s3.py` runs the `test` blocks of `s3.sand` when executed directly) |

The ports use the same algorithms and data structures as the Sand programs
(lists, dicts, sets, `dataclass(slots=True)` / `__slots__` classes for structs,
`match` statements for enums), written the way a Python programmer would write
them, with the standard library only. Where Sand semantics differ from
Python's, the ports reproduce Sand's output (e.g. Sand's float formatting
`1e20` / `NaN`, IEEE results instead of exceptions in `calc`, numbers in JSON
being floats in `template`). Notable translation choices:

* `Result`/`?` error handling becomes exceptions.
* `ledger`'s `Monoid`/`Render` traits, which Sand implements for structs and
  builtin types (`int`, `str`, tuples, lists), become
  `functools.singledispatch` functions; the identity element that Sand passes
  implicitly via the trait dictionary is passed explicitly.
* `http_fetch` uses `urllib.request` on a thread pool (one thread per URL, as
  Sand spawns one task per URL); `chat_server` uses asyncio streams; `s3` uses
  `http.client`, `hashlib` and `hmac`.
* Python's `re` module caches compiled patterns, so the ports' per-call
  `re.sub(pattern, ...)` / `re.findall(pattern, ...)` mirror the Sand code's
  per-call `regex.new(...)` but do not recompile.

## Workloads

The examples' own inputs are tiny, so each workload scales them up to take
roughly 0.2-1 s with the release build. Where a program's input is baked into
its source (`life`, `ledger`), the benchmark uses a *variant*: `gen.py` copies
`examples/X.sand` and `python/X.py` and replaces only the input literal, so the
code is otherwise identical (`write_life_bench`, `write_ledger_bench`).

| workload              | what runs                                                                                     |
|-----------------------|-----------------------------------------------------------------------------------------------|
| `life`                | `life.sand -- 12000`: the 8x6 glider board for 12,000 generations (method calls, `%`, nested list indexing) |
| `life_big`            | variant with a random 64x64 board (35% alive), 150 generations                                |
| `calc`                | 40,000 generated lines on stdin: `let`s, nested arithmetic, `^`, `%`, `sqrt/abs/max/min`, and ~3% lines hitting every error path (lexer, parser, interpreter) |
| `adventure`           | 180,000 commands on stdin: a 30-command loop through every room and command (including failures, unknown words, blank lines) repeated 6,000 times, then the winning sequence |
| `template_render`     | a template using every feature (nested `for`, `if/elif/else`, `not`, comparisons, all filters, `loop.*`, list-index paths) over 12,000 generated users |
| `template_compile`    | a 12,000-section generated template (lexing, parsing, quote-aware splitting) rendered against a small data set |
| `ledger`              | variant with 120,000 baked-in expenses (12 categories, 10 years), run with `food`: parse, three generic `summarize`/`concat` reports, category filter |
| `search_index_index`  | `index` of a generated 600-document corpus (450 `.txt`/`.md` files indexed, `.log` skipped; Zipf-distributed synthetic vocabulary, punctuation, stop words) into a fresh index; the index files written by both implementations must be identical |
| `search_index_search` | 7 `search` processes (each loads the whole index JSON), multi-term queries with common, rare, stop and unknown words |
| `search_index_add`    | `add` of 30 already-indexed documents to a copy of the index (each re-index scans and filters every posting list); resulting index files must match |
| `search_index_stats`  | 4 `stats` processes (load + sort all posting lists)                                           |
| `http_fetch`          | 50 URLs fetched concurrently from a local server: 48 generated ~175 KB HTML pages (60 headings, ~600 links of every kind each; one page is reached through a redirect), a 404 and a non-HTML resource |
| `chat_server`         | the server under 24 concurrent clients that each set a nick and send 600 lines (331,200 deliveries), then `/who` and `/quit`; timed from process start until the last client has received everything. The client driver checks per-sender ordering and reports a digest of each client's received lines |
| `s3_put`              | `cli put` of a 64 MB file (SHA-256 of the payload, SigV4 signing, upload) to a fake S3 server |
| `s3_get`              | 4 x `cli get` of the 64 MB object to a file (file hash checked)                              |
| `s3_ls`               | `cli ls` of 8,000 keys, 1,000 per page (8 signed requests, regex-based XML parsing, formatted listing) |
| `s3_small_ops`        | 26 short `cli` processes (put/stat/cat x6, ls, rm x6, stat of a missing key): start-up dominated |

The network servers (`servers.py`) run in threads of the harness process,
which is otherwise idle while a program runs; they are the same for both
implementations. The fake S3 server, like the mock in `tests/examples.rs`,
only checks that requests carry SigV4-shaped headers.

## Profiling a workload

The release profile has `debug = 1`, so macOS `sample` (or Instruments)
shows Rust function names and line numbers. Materialise the inputs and sample
a run, e.g. for `adventure`:

```
python3.14 bench/examples/gen.py /tmp/sand-inputs
target/release/sand run examples/adventure.sand --allow-stdin < /tmp/sand-inputs/adventure.txt > /dev/null &
sample $! 3 1 -mayDie -file /tmp/adventure.sample.txt
```

Make the input bigger than the benchmark's (e.g. `gen.adventure_script(60_000)`)
so the run lasts longer than the sampling window. Natives show up as
`stdlib::core::register_*::{closure}`; `atos -i -o target/release/sand -l
LOAD_ADDRESS ADDR` on a sampled address maps such a closure to its source
line, and so to the builtin's name.

## Results

Apple M-series laptop, `cargo build --release`, CPython 3.14.8, 5 runs each
(ratio = Sand median / CPython median; below 1 means Sand is faster):

| workload            | Sand    | CPython | ratio |
|---------------------|---------|---------|-------|
| life                | 231 ms  | 400 ms  | 0.58  |
| life_big            | 232 ms  | 423 ms  | 0.55  |
| calc                | 218 ms  | 353 ms  | 0.62  |
| adventure           | 160 ms  | 166 ms  | 0.96  |
| template_render     | 208 ms  | 298 ms  | 0.70  |
| template_compile    | 142 ms  | 189 ms  | 0.75  |
| ledger              | 274 ms  | 554 ms  | 0.49  |
| search_index_index  | 191 ms  | 196 ms  | 0.98  |
| search_index_search | 199 ms  | 609 ms  | 0.33  |
| search_index_add    | 137 ms  | 241 ms  | 0.57  |
| search_index_stats  | 134 ms  | 359 ms  | 0.37  |
| http_fetch          | 88 ms   | 211 ms  | 0.42  |
| chat_server         | 484 ms  | 542 ms  | 0.89  |
| s3_put              | 61 ms   | 81 ms   | 0.75  |
| s3_get              | 136 ms  | 241 ms  | 0.56  |
| s3_ls               | 79 ms   | 82 ms   | 0.96  |
| s3_small_ops        | 255 ms  | 983 ms  | 0.26  |

Geometric mean: 0.59. When this harness was written, five workloads were
slower than CPython (`s3_ls` 11.7x, `s3_put` and `adventure` 2.3x,
`search_index_index` 2.0x, `http_fetch` 1.05x); the changes that fixed them
(a regex compile cache, buffered standard output and synchronous buffered
`read_line`, SHA-256 with the CPU's instructions, in-place string appends,
iterator fusion and native iterator sources, a hot file system worker, and
interpreter work) are described in `docs/INTERNALS.md`.

