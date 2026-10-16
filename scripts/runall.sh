#!/bin/sh
set -eu
script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
# Optional service credentials (e.g. model API keys); .env is gitignored.
env_file="${XSHELL_ENV_FILE:-$script_dir/../.env}"
if [ -f "$env_file" ]; then
    set -a
    . "$env_file"
    set +a
fi
exec python3 "$script_dir/runall.py" "$@"
