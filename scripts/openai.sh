#!/bin/sh
# Compatibility wrapper for the unified provider helper.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$script_dir/login.sh" openai "$@"
