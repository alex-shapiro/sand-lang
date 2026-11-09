# Native-to-script callbacks: research

[`report.md`](report.md) is the report. This directory holds everything that
the report cites, so that every result can be reproduced.

- `sources/checked_quotes.md`: every quote that the report prints, with a
  pinned URL. `sources/*.md` hold the research notes.
- `lab/verify_sources.py`: checks each quote against its source
  (`lab/results/sources.txt`).
- `lab/c/`, `lab/py/`, `lab/src/bin/`: experiments with the Lua C API, the
  CPython C API, and Rust embeddings of other runtimes.
- `lab/sort/`: the sort workload in Lua and Python.
- `lab/e15/`: the compile-time handle test.
- `lab/run_all.sh`: runs every experiment. `lab/overhead.sh`: measures the
  prototypes' cost on the `bench/` programs.
- `lab/results/`: the output that the report cites.
- `sandlab/`: Sand experiments with the current API and both prototypes.
- `prototypes/callbacks-p1-p2.patch`: prototype P1 (nested calls) and P2
  (stackless continuations).

## Reproduce

The patch applies to commit `1520314`. Check out that commit's `src/`
before step 5 if `src/` has changed since.

1. Fetch the crates: `cd lab && cargo fetch`.
2. Build Lua 5.4.8 and 5.5.0 from the `lua-src` crate into `lab/build/lua-<v>/liblua.a`
   (compile every `.c` file except `lua.c`, `luac.c` and `onelua.c` with
   `gcc -O2 -DLUA_USE_LINUX`), then build the harness:
   `gcc -O2 -Ibuild/lua-<v> c/lua_cb.c build/lua-<v>/liblua.a -lm -o build/lua_cb-<v>`.
3. Build the CPython extension for each Python (3.13 and 3.14) into `lab/build/py<v>/`:
   `gcc -O2 -shared -fPIC -I<include> py/cbx.c -o build/py<v>/cbx<EXT_SUFFIX>`.
4. `cd lab && cargo build --release`.
5. `cd sandlab && ./build.sh`. It builds `sand_current`, applies the
   prototype patch to the working tree, and builds the other experiments.
6. Run `lab/run_all.sh` on an otherwise idle machine.
7. Build `sand` three times (unpatched, patched, patched with
   `--features sandlang/nested_callbacks`) and run
   `lab/overhead.sh BASE P2 P1`.

The machine for the cited runs: Linux 6.18 x86_64, 4 vCPUs, rustc 1.97.0,
gcc 13.3, CPython 3.13.18 and 3.14.6.
