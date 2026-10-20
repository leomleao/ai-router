#!/bin/sh
# Operator controls for router-owned Antigravity OAuth state.
# This helper never reads, copies, or prints credential files.
set -eu

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/antigravity.sh login [--port N]
  scripts/antigravity.sh status
  scripts/antigravity.sh logout
  scripts/antigravity.sh resume
EOF
  exit 2
}

[ "$#" -ge 1 ] || usage
command_name=$1
shift
image_name=${AI_ROUTER_IMAGE:-ai-router:local}
volume_name=${AI_ROUTER_ANTIGRAVITY_VOLUME:-ai-router-antigravity-auth}
container_name=${AI_ROUTER_CONTAINER:-ai-router}
port=${AI_ROUTER_ANTIGRAVITY_CALLBACK_PORT:-1456}
case "$port" in ''|*[!0-9]*) usage ;; esac
[ "$port" -ge 1 ] && [ "$port" -le 65535 ] || usage

case "$command_name" in
  login)
    while [ "$#" -gt 0 ]; do
      case "$1" in
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
    set -- antigravity login --port "$port"
    mutates_state=1
    ;;
  status|logout|resume)
    [ "$#" -eq 0 ] || usage
    set -- antigravity "$command_name"
    case "$command_name" in
      status) mutates_state=0 ;;
      *) mutates_state=1 ;;
    esac
    ;;
  *) usage ;;
esac

if [ "$mutates_state" -eq 1 ]; then
  status=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  if [ "$status" = running ]; then
    echo "Stop $container_name before $command_name so account state cannot change during inference." >&2
    echo "Use: docker compose --env-file /dev/null stop ai-router" >&2
    exit 1
  fi
fi

interactive=
[ "$command_name" != login ] || interactive=-it
exec docker run --rm ${interactive:+'-it'} --name ai-router-antigravity-helper --init \
  --user 10001:10001 --read-only --cap-drop ALL \
  --security-opt no-new-privileges \
  --mount "type=volume,source=$volume_name,target=/var/lib/ai-router/antigravity-auth" \
  --publish "127.0.0.1:$port:$port" \
  --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
  --env HOME=/var/lib/ai-router/antigravity-auth \
  --env AI_ROUTER_STATE_DIR=/var/lib/ai-router/antigravity-auth \
  --env AI_ROUTER_GOOGLE_CLIENT_ID \
  --env AI_ROUTER_GOOGLE_CLIENT_SECRET \
  --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
