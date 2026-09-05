#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo fmt -p renet-cross --check
cargo test -p renet-cross --all-features --locked
cargo test -p renet-cross --no-default-features --locked
for transport_features in native-sync native-async axum; do
    cargo check -p renet-cross --no-default-features --features "$transport_features" --locked
done
cargo clippy -p renet-cross --lib --tests --all-features --locked -- -D warnings
cargo clippy -p renet-cross --target wasm32-unknown-unknown --no-default-features --locked -- -D warnings
cargo check -p renet-cross --target wasm32-unknown-unknown --locked
cargo clippy -p renet-cross --target wasm32-unknown-unknown --no-default-features --features packet-conditioner --locked -- -D warnings
cargo check -p renet-cross --target wasm32-unknown-unknown --no-default-features --features bevy-debug-ui --locked
git diff --check
