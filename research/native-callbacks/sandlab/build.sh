#!/usr/bin/env bash
# Builds the Sand experiment binaries into bin/.
#   sand_current: unpatched sandlang (run first, before applying the patch)
#   sand_stackless, sand_nested_noparking: with ../prototypes/callbacks-p1-p2.patch
#   sand_nested: with the patch and the `nested_callbacks` feature (fiber parking)
set -eu
cd "$(dirname "$0")"
mkdir -p bin
root=../../..
if git -C $root apply --check -R ../research/native-callbacks/prototypes/callbacks-p1-p2.patch 2>/dev/null \
   || git -C $root apply --check -R research/native-callbacks/prototypes/callbacks-p1-p2.patch 2>/dev/null; then
  echo "patch already applied: build sand_current from an unpatched tree" >&2
else
  cargo build --release --bin sand_current && cp target/release/sand_current bin/
  git -C $root apply research/native-callbacks/prototypes/callbacks-p1-p2.patch
fi
cargo build --release --bin sand_stackless --bin sand_nested
cp target/release/sand_stackless bin/
cp target/release/sand_nested bin/sand_nested_noparking
cargo build --release --features nested --bin sand_nested
cp target/release/sand_nested bin/
