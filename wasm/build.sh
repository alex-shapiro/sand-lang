#!/bin/sh
# Builds the engine for JavaScript hosts: compiles `sandlang-wasm` for
# wasm32-unknown-unknown (the `wasm-release` profile), generates its
# JavaScript bindings with wasm-bindgen, and optimizes it with wasm-opt
# (binaryen) if installed (in CI, where $CI is set, it must be).
#
#   wasm/build.sh web OUT_DIR [--dev]       ES module, for browsers (the playground)
#   wasm/build.sh nodejs OUT_DIR [--dev]    CommonJS, for Node.js (wasm/tests)
#
# Needs the wasm32-unknown-unknown target (rustup target add
# wasm32-unknown-unknown) and wasm-bindgen-cli of exactly the version of the
# wasm-bindgen crate in Cargo.lock; the script says how to install it.
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

# Cargo's target directory (CARGO_TARGET_DIR or build.target-dir may move it),
# with JSON's escaped backslashes (Windows paths) turned into slashes.
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
