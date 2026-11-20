#!/usr/bin/env bash
# Build the .wasm into dist/. Needs the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
# Runs wasm-opt (binaryen) as well when it is on PATH.
set -euo pipefail
crate="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$crate/../.." && pwd)"

cargo build --manifest-path "$root/Cargo.toml" -p ch-core-wasm \
  --target wasm32-unknown-unknown --profile wasm-release
mkdir -p "$crate/dist"
out="$crate/dist/ch_core_wasm.wasm"
cp "$root/target/wasm32-unknown-unknown/wasm-release/ch_core_wasm.wasm" "$out"

if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int \
    --enable-sign-ext --enable-mutable-globals "$out" -o "$out"
fi
ls -l "$out"
