#!/usr/bin/env bash
# Counts `unsafe` blocks, fns and impls in each runtime's src/, and checks
# for `forbid(unsafe_code)` in lib.rs. Run after `cargo fetch`.
REG=$(ls -d ~/.cargo/registry/src/*/)
for c in piccolo-0.3.3 gc-arena-0.5.3 wasmi-2.0.0 rhai-1.27.0 rune-0.14.2 starlark-0.14.2 mlua-0.11.6 rquickjs-core-0.14.0 wasmtime-internal-fiber-50.0.1; do
  n=$(grep -rE "unsafe (\{|fn|impl)" "$REG/$c/src" | wc -l)
  f=$(grep -l "forbid(unsafe_code)" "$REG/$c/src/lib.rs" >/dev/null 2>&1 && echo yes || echo no)
  printf "%-32s unsafe blocks/fns/impls: %5d   forbid(unsafe_code) in lib.rs: %s\n" "$c" "$n" "$f"
done
root=$(cd "$(dirname "$0")/../../.." && pwd)
n=$(grep -rE "unsafe (\{|fn|impl)" "$root/src" | wc -l)
f=$(grep -q "forbid(unsafe_code)" "$root/src/lib.rs" && echo yes || echo no)
printf "%-32s unsafe blocks/fns/impls: %5d   forbid(unsafe_code) in lib.rs: %s\n" "sandlang (this tree)" "$n" "$f"
