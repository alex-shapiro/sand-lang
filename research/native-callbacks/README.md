# Native-to-script callbacks: research

[`report.md`](report.md) is the report. This directory holds everything that
the report cites, so that every result can be reproduced.

| Path                                   | Contents                                                                 |
| -------------------------------------- | ------------------------------------------------------------------------ |
| `sources/`                             | Primary-source quotes, by runtime family, with URLs                      |
| `lab/verify_sources.py`                | Downloads each source and checks each quote (`lab/results/sources.txt`)  |
| `lab/c/`, `lab/py/`, `lab/src/bin/`    | Experiments: Lua C API, CPython C API, Rust embeddings of other runtimes |
| `lab/sort/`                            | Sort-with-comparator workload in Lua, Python, and Sand                   |
| `lab/run_all.sh`, `lab/overhead.sh`    | Run all experiments; measure prototype overhead on `bench/` programs     |
| `lab/results/`                         | Output of the runs that the report cites                                 |
| `sandlab/`                             | Sand experiments: the current API and the two prototypes                 |
| `prototypes/callbacks-p1-p2.patch`     | Prototype P1 (nested calls) and P2 (stackless continuations)             |

## Reproduce

1. Fetch the crates: `cd lab && cargo fetch`.
2. Build Lua 5.4.8 and 5.5.0 from the `lua-src` crate into `lab/build/lua-<v>/liblua.a`
   (compile every `.c` file except `lua.c`, `luac.c` and `onelua.c` with
   `gcc -O2 -DLUA_USE_LINUX`), then build the harness:
   `gcc -O2 -Ibuild/lua-<v> c/lua_cb.c build/lua-<v>/liblua.a -lm -o build/lua_cb-<v>`.
3. Build the CPython extension for each Python (3.13 and 3.14) into `lab/build/py<v>/`:
   `gcc -O2 -shared -fPIC -I<include> py/cbx.c -o build/py<v>/cbx<EXT_SUFFIX>`.
4. `cd lab && cargo build --release`.
5. `cd sandlab && ./build.sh` (applies the prototype patch to the working tree).
6. `lab/run_all.sh` on a quiet machine.
7. Build `sand` three times (unpatched, patched, patched with
   `--features sandlang/nested_callbacks`) and run
   `lab/overhead.sh BASE P2 P1`.

The machine for the cited runs: Linux 6.18 x86_64, 4 vCPUs, rustc 1.97.0,
gcc 13.3, CPython 3.13.18 and 3.14.6.
