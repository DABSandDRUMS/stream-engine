#!/usr/bin/env bash
# Build src/lib.rs into main.wasm (the entry named in patch.toml).
# Needs the wasm32 target: `rustup target add wasm32-unknown-unknown`.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown
out=target/wasm32-unknown-unknown/release/patch.wasm
if command -v wasm-opt >/dev/null 2>&1; then
    wasm-opt -O3 --enable-bulk-memory --enable-sign-ext --enable-nontrapping-float-to-int \
        --enable-mutable-globals --enable-multivalue --enable-reference-types "$out" -o main.wasm
else
    cp "$out" main.wasm
fi
echo "built $(pwd)/main.wasm ($(wc -c < main.wasm) bytes)"
