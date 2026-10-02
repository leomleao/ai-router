#!/bin/sh
# Generate inside the image without requiring the gateway service or AGY login.
# Output contains a new raw key: the operator must save it securely.
set -eu
exec docker run --rm --network none --read-only --cap-drop ALL \
  --security-opt no-new-privileges --user 10001:10001 \
  --entrypoint /usr/local/bin/ai-router \
  "${AI_ROUTER_IMAGE:-ai-router:local}" keygen "$@"
