#!/bin/sh
# Operator-only; never reads or copies saved credentials.
set -eu
container_name=${AI_ROUTER_CONTAINER:-ai-router}
if [ "$#" -gt 1 ]; then
  echo 'Usage: scripts/login.sh [--remote]' >&2
  exit 2
fi
if [ "${1:-}" = '--remote' ]; then
  # AGY's documented browser/code flow is selected by its SSH detection. Forward
  # an actual SSH session when present, or mark this container session remote.
  exec docker exec -it --user 10001:10001 \
    --workdir /var/lib/ai-router/auth \
    -e HOME=/var/lib/ai-router/auth \
    -e TMPDIR=/run/ai-router \
    -e "SSH_CONNECTION=${SSH_CONNECTION:-127.0.0.1 1 127.0.0.1 22}" \
    "$container_name" agy
fi
if [ "$#" -ne 0 ]; then
  echo 'Usage: scripts/login.sh [--remote]' >&2
  exit 2
fi
exec docker exec -it --user 10001:10001 \
  --workdir /var/lib/ai-router/auth \
  -e HOME=/var/lib/ai-router/auth -e TMPDIR=/run/ai-router "$container_name" agy
