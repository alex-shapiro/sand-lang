#!/usr/bin/env bash
# Runs every experiment serially and writes results/*.txt. Run on a quiet machine.
# Prerequisites: build/ (see README), `cargo build --release` here and in ../sandlab.
set -u
cd "$(dirname "$0")"
mkdir -p results
run() { local out=$1; shift; { "$@"; echo "exit=$?"; } > "results/$out.txt" 2>&1; }
for v in 5.4.8 5.5.0; do run lua-$v ./build/lua_cb-$v c/lua_cb.lua; done
PY314=$(uv python find 3.14 2>/dev/null)
for py in /usr/local/bin/python3.13 "$PY314"; do
  v=$($py -c 'import sys;print(f"{sys.version_info[0]}.{sys.version_info[1]}")')
  run python-$v $py -I py/cb.py build/py$v
  run python-$v-depth $py -I py/cb_depth.py build/py$v
done
for b in wasmtime_cb wasmtime_async_cb wasmi_cb quickjs_cb piccolo_cb rhai_cb mlua_cb starlark_cb rune_cb; do run $b timeout 300 ./target/release/$b; done
run rhai_cb_deep timeout 300 ./target/release/rhai_cb deep
run rune_cb_budget timeout 60 ./target/release/rune_cb budget
for v in 5.4.8 5.5.0; do run sort_lua-$v ./build/lua_cb-$v sort/sort.lua; done
run sort_python-3.13 /usr/local/bin/python3.13 -I sort/sort.py
run sort_python-3.14 "$PY314" -I sort/sort.py
run e15 ./e15/run.sh
run unsafe_count ./unsafe_count.sh
S=../sandlab/bin
run sand_current $S/sand_current
run sand_host env RUST_BACKTRACE=0 $S/sand_host
run sand_p2 $S/sand_stackless
run sand_p1 $S/sand_nested
run sand_p1_noparking $S/sand_nested_noparking gc
