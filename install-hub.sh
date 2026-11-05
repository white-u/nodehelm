#!/bin/sh
# NodeHelm only: never fetch or overwrite the upstream monitor installation.
set -eu
BASE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$BASE/scripts/hub-install.py" "$@"
