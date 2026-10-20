#!/bin/sh
# Compatibility entry point for the direct Antigravity OAuth helper.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$script_dir/antigravity.sh" login "$@"
