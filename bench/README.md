# Benchmarks: Sand vs Lua 5.5 and CPython 3.14

`run.py` runs two suites, checks that every implementation prints exactly
what Sand prints, and prints a timing table:

- **programs** (`programs/`): self-contained programs, each written in Sand,
  Lua and Python with the same algorithm and data structures, in each
  language's idiom. They generate their own inputs and target the areas where
  an interpreter like Sand's tends to be weak: calls, closures, hash maps
  keyed by ints, strings and tuples, strings, allocation, `Result`/`Option`.
- **examples**: the programs in `examples/` on scaled-up generated inputs,
  against their ports in `ports/python/` and `ports/lua/` (Lua's standard
  library has no sockets, crypto or directory listing, so `search_index`,
  `http_fetch`, `chat_server` and `s3` have no Lua port).

```
cargo build --release                  # run.py does not build anything
python3.14 bench/run.py                # everything, 5 timed runs each
python3.14 bench/run.py --suite programs --only json,memo --runs 10
```

| option          | meaning                                                                |
| --------------- | ---------------------------------------------------------------------- |
| `--suite S`     | `programs`, `examples` or `all` (default)                              |
| `--only NAMES`  | comma-separated workload names or prefixes (`search_index` = all four) |
| `--runs N`      | timed runs per implementation (default 5)                              |
| `--impls LIST`  | implementations to run (default `sand,lua,python`)                     |
| `--sand PATH`   | Sand binary (default `target/release/sand`)                            |
| `--lua PATH`    | Lua 5.5 interpreter (default `lua5.5`)                                 |
| `--python PATH` | Python 3.14 interpreter (default: the one running `run.py`)            |
| `--any-version` | allow other Lua and Python versions (ratios are then not comparable)   |
| `--keep`        | keep the generated inputs (the temp dir is printed)                    |

Each workload runs once untimed per implementation, then N times,
alternating implementations. Times are medians of whole-process wall-clock
times, including start-up and compilation. Sand runs with
`--max-memory=8G` and exactly the permissions each program needs. A
mismatch prints the first differing line, saves the outputs next to the
inputs, and makes `run.py` exit with status 1.

## Files

| file                          | contents                                                      |
| ----------------------------- | ------------------------------------------------------------- |
| `run.py`                      | harness and the workload definitions                          |
| `gen.py`                      | deterministic input generators for the examples               |
| `servers.py`                  | local HTTP page server, fake S3 server and chat client driver |
| `programs/NAME.{sand,lua,py}` | the programs suite                                            |
| `ports/python/`, `ports/lua/` | ports of the examples (`ports/lua/json.lua`: a JSON module)   |

The ports reproduce Sand's output where semantics differ (float formatting
`1e20` / `NaN`, IEEE results in `calc`, JSON numbers as floats in
`template`). Notable translation choices:

- `Result`/`?` becomes exceptions in Python and `nil, err` returns or
  `error`/`pcall` in Lua.
- `ledger`'s `Monoid`/`Render` traits become `functools.singledispatch`
  functions in Python and functions dispatching on the value's type or
  metatable in Lua; the identity element that Sand passes implicitly via
  the trait dictionary is passed explicitly.
- `http_fetch` uses `urllib.request` on a thread pool (one thread per URL, as
  Sand spawns one task per URL); `chat_server` uses asyncio streams; `s3` uses
  `http.client`, `hashlib` and `hmac`.
- Python's `re` module caches compiled patterns, so the ports' per-call
  `re.sub(pattern, ...)` mirror the Sand code's per-call `regex.new(...)` but
  do not recompile. Lua has no regex: `template.lua` scans with `string.find`.
- Lua ports that print many lines write with `io.write` (`print` flushes
  every line).

## Profiling a workload

The `profiling` profile is the release profile with `debug = 1`, so macOS
`sample`, Instruments or `xctrace` show Rust function names and lines:

```
cargo build --profile profiling
target/profiling/sand run bench/programs/json.sand > /dev/null &
sample $! 3 1 -mayDie -file /tmp/json.sample.txt
```

`SAND_DUMP_BYTECODE=1` prints the compiled code and `SAND_GC_STATS=1` the
collector's statistics. For the examples, `python3.14 bench/gen.py DIR`
writes all generated inputs.

## Results

Apple M1, `cargo build --release`, Lua 5.5.1, CPython 3.14.8, 5 runs each.
Ratios are Sand's time over the other's: below 1, Sand is faster.

| workload              |  Sand |    Lua | CPython |     /Lua | /CPython |
| --------------------- | ----: | -----: | ------: | -------: | -------: |
| `binary_trees`        | 457ms |  669ms |  1154ms |     0.68 |     0.40 |
| `collatz`             | 324ms |  335ms |  1233ms |     0.97 |     0.26 |
| `csv`                 | 338ms |  537ms |   637ms |     0.63 |     0.53 |
| `dijkstra`            | 643ms |  926ms |   657ms |     0.69 |     0.98 |
| `fannkuch`            | 206ms |  403ms |   365ms |     0.51 |     0.56 |
| `generators`          | 527ms |  955ms |  1460ms |     0.55 |     0.36 |
| `graph`               | 554ms |  864ms |  1417ms |     0.64 |     0.39 |
| `interpreter`         | 521ms |  688ms |  1801ms |     0.76 |     0.29 |
| `json`                | 794ms |  871ms |   933ms |     0.91 |     0.85 |
| `knucleotide`         | 484ms |  742ms |   723ms |     0.65 |     0.67 |
| `memo`                | 573ms |  618ms |  1469ms |     0.93 |     0.39 |
| `nbody`               | 702ms |  986ms |  1945ms |     0.71 |     0.36 |
| `pipeline`            | 354ms |  745ms |   929ms |     0.48 |     0.38 |
| `ray`                 | 340ms | 1001ms |  1088ms |     0.34 |     0.31 |
| `results`             | 759ms | 1290ms |  1091ms |     0.59 |     0.70 |
| `richards`            | 472ms |  798ms |   825ms |     0.59 |     0.57 |
| `spectral_norm`       | 747ms | 1052ms |  2930ms |     0.71 |     0.25 |
| `wordfreq`            | 645ms |  871ms |   773ms |     0.74 |     0.83 |
| `life`                | 210ms |  402ms |   595ms |     0.52 |     0.35 |
| `life_big`            | 210ms |  392ms |   627ms |     0.54 |     0.34 |
| `calc`                | 215ms |  396ms |   483ms |     0.54 |     0.45 |
| `adventure`           | 180ms |  245ms |   231ms |     0.73 |     0.78 |
| `template_render`     | 230ms |  440ms |   422ms |     0.52 |     0.55 |
| `template_compile`    | 179ms |  227ms |   269ms |     0.79 |     0.66 |
| `ledger`              | 329ms |  761ms |   828ms |     0.43 |     0.40 |
| `search_index_index`  | 223ms |      - |   306ms |        - |     0.73 |
| `search_index_search` | 288ms |      - |  1019ms |        - |     0.28 |
| `search_index_add`    | 189ms |      - |   427ms |        - |     0.44 |
| `search_index_stats`  | 181ms |      - |   592ms |        - |     0.31 |
| `http_fetch`          | 171ms |      - |   321ms |        - |     0.53 |
| `chat_server`         | 687ms |      - |   874ms |        - |     0.79 |
| `s3_put`              |  91ms |      - |   115ms |        - |     0.79 |
| `s3_get`              | 263ms |      - |   430ms |        - |     0.61 |
| `s3_ls`               | 133ms |      - |   127ms |        - |     1.05 |
| `s3_small_ops`        | 558ms |      - |  1527ms |        - |     0.37 |
| **geomean programs**  |       |        |         | **0.65** | **0.46** |
| **geomean examples**  |       |        |         | **0.57** | **0.52** |

The first 18 rows are the programs suite, the rest the examples.
