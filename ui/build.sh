#!/usr/bin/env bash
# Build the hub's web UI into ui/dist: the app for wasm32, its JavaScript
# glue by wasm-bindgen-cli (the version Cargo.toml pins), and the static
# files. In the dev shell: `ui/build.sh`, then `ensemble-hub serve --ui-dir
# ui/dist`. The Nix package builds it the same way and embeds it in the hub.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown
rm -rf dist
wasm-bindgen --target web --no-typescript --out-dir dist --out-name ensemble_ui \
  target/wasm32-unknown-unknown/release/ensemble-ui.wasm
cp static/* dist/
