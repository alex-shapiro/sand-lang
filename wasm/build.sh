#!/bin/sh
# Builds the `sandlang-wasm` crate for `wasm32-unknown-unknown`, generates
# JavaScript bindings with `wasm-bindgen`, and optimizes the output with
# `wasm-opt` in release builds.
#
#   wasm/build.sh web OUT_DIR [--dev]       ES module for browsers
#   wasm/build.sh nodejs OUT_DIR [--dev]    CommonJS module for Node.js tests
#
# Requires the `wasm32-unknown-unknown` target and a `wasm-bindgen-cli` binary
# whose version matches the `wasm-bindgen` crate in Cargo.lock.
set -eu

usage() {
    echo "usage: wasm/build.sh web|nodejs OUT_DIR [--dev]" >&2
    exit 2
}
[ $# -ge 2 ] || usage
target=$1
out=$2
case "$target" in web | nodejs) ;; *) usage ;; esac
profile=release
case "${3:-}" in
"") ;;
--dev) profile=dev ;;
*) usage ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
cd "$root"

# The wasm-bindgen command must match the crate exactly.
want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
have=$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)
if [ "$have" != "$want" ]; then
    if [ -z "$have" ]; then
        echo "wasm/build.sh: wasm-bindgen is not installed." >&2
    else
        echo "wasm/build.sh: wasm-bindgen is $have; this build needs $want." >&2
    fi
    echo "Install it with: cargo install wasm-bindgen-cli --version $want --locked" >&2
    exit 1
fi

# Normalize backslashes in Cargo's target_directory on Windows.
target_dir=$(cargo metadata --format-version 1 --no-deps |
    sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\//g')
build="$target_dir/wasm32-unknown-unknown"
if [ "$profile" = release ]; then
    cargo build -p sandlang-wasm --target wasm32-unknown-unknown --profile wasm-release
    wasm-bindgen --target "$target" --remove-name-section --remove-producers-section --out-dir "$out" \
        "$build/wasm-release/sandlang_wasm.wasm"
else
    cargo build -p sandlang-wasm --target wasm32-unknown-unknown
    wasm-bindgen --target "$target" --out-dir "$out" "$build/debug/sandlang_wasm.wasm"
fi

module="$out/sandlang_wasm_bg.wasm"
if [ "$profile" = release ]; then
    if command -v wasm-opt >/dev/null 2>&1; then
        wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext "$module" -o "$module"
    elif [ -n "${CI:-}" ]; then
        echo "wasm/build.sh: wasm-opt (binaryen) is not installed." >&2
        exit 1
    else
        echo "wasm/build.sh: wasm-opt (binaryen) is not installed; the module is not optimized further." >&2
    fi
fi
echo "Built $out ($(wc -c <"$module" | tr -d ' ') bytes of WebAssembly)."
