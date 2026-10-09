#!/bin/sh
# Builds the engine for the playground into playground/pkg (see wasm/build.sh).
#
#   playground/build.sh            optimized build
#   playground/build.sh --dev      faster to build, slower to run
#
# Then open it: sand playground   (or serve this directory with any static file server)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
"$here/../wasm/build.sh" web "$here/pkg" "$@"
echo "Open it with: sand playground   (or any static file server)"
