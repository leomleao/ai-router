#!/bin/sh
set -eu
container_name=${AI_ROUTER_CONTAINER:-ai-router}
# Avoid forcing a TTY for JSON/text snapshots in pipes or scheduled commands.
if [ -t 0 ] && [ -t 1 ]; then
  exec docker exec -it --user 10001:10001 "$container_name" ai-router monitor "$@"
fi
exec docker exec -i --user 10001:10001 "$container_name" ai-router monitor "$@"
