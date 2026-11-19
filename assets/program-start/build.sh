#!/bin/sh
# Rebuilds the objects the engine links into student programs (see src/worker/mod.rs).
# Needs a WASI SDK; pass its path, e.g. ./build.sh ~/wasi-sdk-32.0-arm64-macos
set -eu
sdk=$1
cd "$(dirname "$0")"
for name in cwd stdio; do
  "$sdk/bin/clang" --target=wasm32-wasip1 --sysroot="$sdk/share/wasi-sysroot" -O2 -c "$name.c" -o "$name.o"
done
