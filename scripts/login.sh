#!/bin/sh
# Unified operator controls for the router-owned OAuth sessions.
# This helper never reads, copies, or prints credential files.
set -eu

usage() {
  cat <<'EOF'
Usage:
  login.sh agy [login|status|logout|resume] [--port N]
  login.sh openai [login|status|accounts|logout|resume|select ID]
                         [--new] [--account ID] [--port N]

With no action, login is selected. OpenAI login supports --new, --account ID,
and --port N. Callback ports default to 1456 (agy) and 1455 (OpenAI).
EOF
}
fail_usage() { usage >&2; exit 2; }

[ "$#" -gt 0 ] || fail_usage
provider=$1
shift
case "$provider" in agy|openai) ;; -h|--help|help) usage; exit 0 ;; *) fail_usage ;; esac

action=login
case "${1-}" in
  login|status|accounts|logout|resume|select) action=$1; shift ;;
  -h|--help|help) usage; exit 0 ;;
esac

image_name=${AI_ROUTER_IMAGE:-ai-router:local}
container_name=${AI_ROUTER_CONTAINER:-ai-router}
if [ "$provider" = agy ]; then
  binary_provider=antigravity
  volume_name=${AI_ROUTER_ANTIGRAVITY_VOLUME:-ai-router-antigravity-auth}
  target=/var/lib/ai-router/antigravity-auth
  port=${AI_ROUTER_ANTIGRAVITY_CALLBACK_PORT:-1456}
else
  binary_provider=openai
  volume_name=${AI_ROUTER_OPENAI_VOLUME:-ai-router-openai-auth}
  target=/var/lib/ai-router/openai-auth
  port=${AI_ROUTER_OPENAI_CALLBACK_PORT:-1455}
fi
check_port() {
  case "$1" in ''|*[!0-9]*) fail_usage ;; esac
  [ "$1" -ge 1 ] && [ "$1" -le 65535 ] || fail_usage
}
check_port "$port"

fresh=0
account=''
selected=''
port_option=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --port)
      [ "$#" -ge 2 ] || fail_usage
      [ "$port_option" -eq 0 ] || fail_usage
      check_port "$2"
      port=$2
      port_option=1
      shift 2
      ;;
    --new)
      [ "$provider" = openai ] && [ "$action" = login ] && [ "$fresh" -eq 0 ] || fail_usage
      fresh=1
      shift
      ;;
    --account)
      [ "$provider" = openai ] && [ "$action" = login ] && [ "$#" -ge 2 ] && [ -z "$account" ] || fail_usage
      [ -n "$2" ] || fail_usage
      account=$2
      shift 2
      ;;
    *)
      if [ "$provider" = openai ] && [ "$action" = select ] && [ -z "$selected" ]; then
        selected=$1
        shift
      else
        fail_usage
      fi
      ;;
  esac
done
[ "$action" != select ] || [ -n "$selected" ] || fail_usage
[ "$provider" != agy ] || { [ "$action" != accounts ] && [ "$action" != select ]; } || fail_usage
[ "$provider" != agy ] || [ "$fresh" -eq 0 ] || fail_usage
[ "$provider" != agy ] || [ -z "$account" ] || fail_usage
[ "$fresh" -eq 0 ] || [ -z "$account" ] || fail_usage
[ "$action" = login ] || { [ "$fresh" -eq 0 ] && [ -z "$account" ] && [ "$port_option" -eq 0 ]; } || fail_usage

is_running() {
  current_status=$(docker inspect --format '{{.State.Status}}' "$container_name" 2>/dev/null || true)
  case "$current_status" in running|paused|restarting) return 0 ;; *) return 1 ;; esac
}
require_stopped() {
  if is_running; then
    echo "Stop $container_name before $action so account state cannot change during inference." >&2
    echo "Use: docker stop \"$container_name\"" >&2
    exit 1
  fi
}

set -- "$binary_provider" "$action"
case "$action" in
  login)
    if [ "$provider" = agy ]; then
      set -- "$@" --port "$port"
    else
      set -- "$@" --listen 0.0.0.0 --port "$port"
      [ "$fresh" -eq 0 ] || set -- "$@" --new
      [ -z "$account" ] || set -- "$@" --account "$account"
    fi
    require_stopped
    docker_args="login"
    ;;
  status|accounts)
    [ "$action" != accounts ] || [ "$provider" = openai ] || fail_usage
    docker_args=readonly
    ;;
  select)
    [ "$provider" = openai ] || fail_usage
    set -- "$@" "$selected"
    require_stopped
    docker_args=mutation
    ;;
  logout|resume)
    require_stopped
    docker_args=mutation
    ;;
esac

if [ "$docker_args" = readonly ] && is_running; then
  exec docker exec --user 10001:10001 "$container_name" /usr/local/bin/ai-router "$@"
fi
run_common() {
  docker run --pull never --rm --name "ai-router-$provider-helper" --init \
    --user 10001:10001 --read-only --cap-drop ALL \
    --security-opt no-new-privileges \
    --mount "type=volume,source=$volume_name,target=$target" \
    --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
    --env "HOME=$target" "$@"
}
if [ "$docker_args" = login ]; then
  if [ "$provider" = agy ]; then
    run_common --env "AI_ROUTER_STATE_DIR=$target" --env AI_ROUTER_GOOGLE_CLIENT_ID --env AI_ROUTER_GOOGLE_CLIENT_SECRET \
      -it --publish "127.0.0.1:$port:$port" --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
    exit $?
  fi
  run_common --env "AI_ROUTER_OPENAI_STATE_DIR=$target" \
    -it --publish "127.0.0.1:$port:$port" --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
  exit $?
fi
if [ "$provider" = agy ]; then
  exec docker run --pull never --rm --network none --name ai-router-agy-helper --init \
    --user 10001:10001 --read-only --cap-drop ALL --security-opt no-new-privileges \
    --mount "type=volume,source=$volume_name,target=$target" \
    --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
    --env "HOME=$target" --env "AI_ROUTER_STATE_DIR=$target" \
    --env AI_ROUTER_GOOGLE_CLIENT_ID --env AI_ROUTER_GOOGLE_CLIENT_SECRET \
    --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
fi
# Revocation and grant revalidation need outbound HTTPS; no callback port is published.
openai_network=none
case "$action" in logout|resume) openai_network=bridge ;; esac
exec docker run --pull never --rm --network "$openai_network" --name ai-router-openai-helper --init \
  --user 10001:10001 --read-only --cap-drop ALL --security-opt no-new-privileges \
  --mount "type=volume,source=$volume_name,target=$target" \
  --tmpfs /run/ai-router:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m,uid=10001,gid=10001,mode=0700 \
  --env "HOME=$target" --env "AI_ROUTER_OPENAI_STATE_DIR=$target" \
  --entrypoint /usr/local/bin/ai-router "$image_name" "$@"
