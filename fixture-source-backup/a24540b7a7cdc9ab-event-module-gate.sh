#!/usr/bin/env bash
set -euo pipefail
export CARGO_BUILD_JOBS=2
cargo test -p sv-gpu-server --lib native_video_events::tests
cargo check -p sv-gpu-server --target x86_64-apple-darwin
