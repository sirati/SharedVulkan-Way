#!/usr/bin/env bash
set -euo pipefail
export CARGO_BUILD_JOBS=2
cargo check -p sv-gpu-server --target x86_64-apple-darwin
cargo check -p sv-gpu-server
cargo test -p sv-codegen --test gpu_optional_overrides
cargo test -p sv-gpu-server --lib video
