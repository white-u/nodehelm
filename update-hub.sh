#!/bin/sh
# Upgrades the installed hub to the program in this package; options before the
# action pass through for a non-default unit. See `hub-lifecycle.py --help`.
set -eu
BASE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
cd "$BASE"
sha256sum --check --quiet sha256sums.txt
exec python3 scripts/hub-lifecycle.py "$@" upgrade --candidate "$BASE/monitor-hub" \
  --sha256 "$(sha256sum monitor-hub | cut -d' ' -f1)"
