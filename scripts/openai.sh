#!/bin/sh
# Operator-only OpenAI account controls. This script never reads credential files.
set -eu

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/openai.sh login [--new] [--account ID] [--port N]
  scripts/openai.sh status
  scripts/openai.sh accounts
  scripts/openai.sh select ID
  scripts/openai.sh logout
  scripts/openai.sh resume
EOF
  exit 2
}

[ "$#" -ge 1 ] || usage
command_name=$1
shift
container_name=${AI_ROUTER_CONTAINER:-ai-router}
image_name=${AI_ROUTER_IMAGE:-ai-router:local}
volume_name=${AI_ROUTER_OPENAI_VOLUME:-ai-router-openai-auth}
port=${AI_ROUTER_OPENAI_CALLBACK_PORT:-1455}
case "$port" in ''|*[!0-9]*) usage ;; esac
[ "$port" -ge 1 ] && [ "$port" -le 65535 ] || usage

is_gateway_running() {
  status=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  [ "$status" = running ]
}

require_gateway_stopped() {
  if is_gateway_running; then
    echo "Stop $container_name before $command_name so account state cannot change during inference." >&2
    echo "Use: docker compose --env-file /dev/null stop ai-router" >&2
    exit 1
  fi
}

case "$command_name" in
  status|accounts)
    [ "$#" -eq 0 ] || usage
    exec docker exec --user 10001:10001 "$container_name" \
      /usr/local/bin/ai-router openai "$command_name"
    ;;
  login)
    fresh=0
    account=''
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --new)
          [ "$fresh" -eq 0 ] || usage
          fresh=1
          shift
          ;;
        --account)
          [ "$#" -ge 2 ] && [ -z "$account" ] || usage
          account=$2
          shift 2
          ;;
        --port)
          [ "$#" -ge 2 ] || usage
          case "$2" in ''|*[!0-9]*) usage ;; esac
          [ "$2" -ge 1 ] && [ "$2" -le 65535 ] || usage
          port=$2
          shift 2
          ;;
        *) usage ;;
      esac
    done
    require_gateway_stopped
    set -- openai login --listen 0.0.0.0 --port "$port"
    [ "$fresh" -eq 0 ] || set -- "$@" --new
    [ -z "$account" ] || set -- "$@" --account "$account"
    exec docker run --rm -it --name ai-router-openai-helper --init \
      --user 10001:10001 --read-only --cap-drop ALL \
      --security-opt no-new-privileges \
      --mount "type=volume,source=$volume_name,target=/var/lib/ai-router/openai-auth" \
      --publish "127.0.0.1:$port:$port" \
      --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
      --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
      --env HOME=/var/lib/ai-router/openai-auth \
      --env AI_ROUTER_OPENAI_STATE_DIR=/var/lib/ai-router/openai-auth \
      --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
    ;;
  select)
    [ "$#" -eq 1 ] || usage
    account=$1
    require_gateway_stopped
    set -- openai select "$account"
    ;;
  logout|resume)
    [ "$#" -eq 0 ] || usage
    require_gateway_stopped
    set -- openai "$command_name"
    ;;
  *) usage ;;
esac

exec docker run --rm --name ai-router-openai-helper --init \
  --user 10001:10001 --read-only --cap-drop ALL \
  --security-opt no-new-privileges \
  --mount "type=volume,source=$volume_name,target=/var/lib/ai-router/openai-auth" \
  --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
  --env HOME=/var/lib/ai-router/openai-auth \
  --env AI_ROUTER_OPENAI_STATE_DIR=/var/lib/ai-router/openai-auth \
  --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
