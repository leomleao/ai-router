#!/bin/sh
# Docker-only runtime tests. This image contains the synthetic provider only;
# authentication volumes never overlap with the real AGY service volume.
set -eu
test_image=${AI_ROUTER_TEST_IMAGE:-ai-router-test:local}
repo_dir=$(cd "$(dirname "$0")/.." && pwd)
test_suffix="$(date +%s)-$$"
test_container="ai-router-contract-$test_suffix"
test_auth_volume="ai-router-contract-auth-$test_suffix"
test_telemetry_volume="ai-router-contract-telemetry-$test_suffix"
cleanup() {
  docker rm -f "$test_container" >/dev/null 2>&1 || true
  docker volume rm "$test_auth_volume" "$test_telemetry_volume" >/dev/null 2>&1 || true
}
trap cleanup EXIT HUP INT TERM
if [ "${AI_ROUTER_TEST_SKIP_BUILD:-0}" != '1' ]; then
  docker build --target test -t "$test_image" "$repo_dir"
fi
docker volume create "$test_auth_volume" >/dev/null
docker volume create "$test_telemetry_volume" >/dev/null
docker run --rm -i --name "$test_container" --init --network none \
  --user 10001:10001 --read-only --cap-drop ALL \
  --security-opt no-new-privileges \
  --mount "type=volume,source=$test_auth_volume,target=/var/lib/ai-router/auth" \
  --mount "type=volume,source=$test_telemetry_volume,target=/var/lib/ai-router/telemetry" \
  --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=256m,uid=10001,gid=10001,mode=0700 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=64m,uid=10001,gid=10001,mode=0700 \
  --entrypoint /opt/test-venv/bin/python "$test_image" - < "$repo_dir/tests/client_contract.py"
