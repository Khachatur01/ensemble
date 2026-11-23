#!/usr/bin/env bash
# Build the web UI's app into ui/dist: the app for wasm32, its JavaScript glue
# by wasm-bindgen-cli (the version app/Cargo.toml pins), and its static files.
# In the dev shell: `ui/build.sh`, then
# `cargo run --manifest-path ui/Cargo.toml -- --hub http://127.0.0.1:7700 --dist ui/dist`.
# The Nix package builds it the same way and embeds it in ensemble-ui.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release -p ensemble-ui-app --target wasm32-unknown-unknown
rm -rf dist
wasm-bindgen --target web --no-typescript --out-dir dist --out-name ensemble_ui \
  target/wasm32-unknown-unknown/release/ensemble-ui-app.wasm
cp app/static/* dist/
