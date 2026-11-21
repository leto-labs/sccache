#!/usr/bin/env bash
# Runs a private measurement, soak or test run in a transient systemd user
# scope with a memory cap, no swap, a task cap and a timeout, then stops the
# scope so that nothing it started (private daemons, stray clients) survives.
# A runaway measurement once exhausted worker-01's 251 GB.
#
# Usage: leto/bounded.sh [--mem 8G] [--tasks 4096] [--timeout SECONDS] -- COMMAND...
#   Defaults: LETO_MEM_MAX (8G), LETO_TASKS_MAX (4096), LETO_TIMEOUT (3600 s).
#   Raising the memory cap above 8G needs Core's approval.
#   --scope-unit NAME.scope lets a caller supervise an idle scope and collect
#   its result. Timeout becomes the systemd runtime limit. The caller stops it.
set -euo pipefail

mem=${LETO_MEM_MAX:-8G}
tasks=${LETO_TASKS_MAX:-4096}
limit=${LETO_TIMEOUT:-3600}
scope_unit=
while [[ $# -gt 0 ]]; do
  case $1 in
    --mem) mem=$2; shift 2 ;;
    --tasks) tasks=$2; shift 2 ;;
    --timeout) limit=$2; shift 2 ;;
    --scope-unit) scope_unit=$2; shift 2 ;;
    --) shift; break ;;
    *) break ;;
  esac
done
[[ $# -gt 0 ]] || { echo "usage: leto/bounded.sh [--mem 8G] [--tasks N] [--timeout S] -- COMMAND..." >&2; exit 2; }

unit=${scope_unit:-leto-bounded-$$-$(date +%s).scope}
scope=(systemd-run --user --scope --quiet --unit="$unit"
  -p MemoryMax="$mem" -p MemorySwapMax=0 -p TasksMax="$tasks")
if [[ -n $scope_unit ]]; then
  export LETO_BOUNDED=1
  exec "${scope[@]}" -p OOMPolicy=kill -p CPUWeight=idle \
    -p TimeoutStopSec=5s -p RuntimeMaxSec="${limit}s" -- "$@"
fi

stop() { systemctl --user stop "$unit" >/dev/null 2>&1 || true; }
trap stop EXIT
trap 'stop; exit 130' INT TERM
status=0
started=$SECONDS
LETO_BOUNDED=1 "${scope[@]}" -- timeout -k 30 "$limit" "$@" || status=$?
if [[ $status -ne 0 ]]; then
  # An OOM kill inside the scope can also surface as 124, so judge by time.
  if (( SECONDS - started >= limit )); then
    echo "leto/bounded.sh: timed out after ${limit}s: $*" >&2
  else
    echo "leto/bounded.sh: exit $status after $((SECONDS - started))s (memory cap $mem or a signal): $*" >&2
  fi
fi
exit "$status"
