#!/usr/bin/env bash
# Builds the Sand experiment binaries into bin/.
#   sand_current, sand_host: unpatched Sand
#   sand_stackless, sand_nested_noparking: with ../prototypes/callbacks-p1-p2.patch
#   sand_nested: with the patch and the `nested_callbacks` feature (fiber parking)
# The patch applies to commit 1520314. The script applies it to the working
# tree of this repository, so start from a clean tree at that commit's src/.
set -eu
cd "$(dirname "$0")"
mkdir -p bin
root=$(git rev-parse --show-toplevel)
patch=research/native-callbacks/prototypes/callbacks-p1-p2.patch
if git -C "$root" apply --check -R "$patch" 2>/dev/null; then
  echo "the patch is already applied: build sand_current and sand_host from an unpatched tree first" >&2
else
  if ! git -C "$root" diff --quiet 1520314 -- src Cargo.toml; then
    echo "src/ or Cargo.toml differs from commit 1520314; the patch may not apply" >&2
    exit 1
  fi
  cargo build --release --bin sand_current --bin sand_host
  cp target/release/sand_current target/release/sand_host bin/
  git -C "$root" apply "$patch"
fi
cargo build --release --bin sand_stackless --bin sand_nested
cp target/release/sand_stackless bin/
cp target/release/sand_nested bin/sand_nested_noparking
SANDLAB_PARKING=1 cargo build --release --features sandlang/nested_callbacks --bin sand_nested
cp target/release/sand_nested bin/
