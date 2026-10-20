#!/bin/sh
# Deterministic Rust, SDK and PTY tests run inside the isolated Docker test stage.
# Build context exclusions prevent protected files from being transferred.
set -eu
repo_dir=$(cd "$(dirname "$0")/.." && pwd)
test_image=${AI_ROUTER_TEST_IMAGE:-ai-router-test:direct}
exec docker build --target test -t "$test_image" "$repo_dir"
