#!/usr/bin/env bash
# Runs a command (default: the CI test set) in a private network namespace.
# The upstream test harness strips every SCCACHE_* variable, including the
# port, and sends --stop-server, which on a shared host would reach the shared
# server on 127.0.0.1:4226. With --unshare-net the namespace has its own
# loopback, so nothing outside it can be reached or stopped.
#
# Usage: leto/test-sandboxed.sh [command ...]
#   leto/test-sandboxed.sh cargo test --locked --test system -- basedirs
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")/.."
tmp=$(mktemp -d "${TMPDIR:-/tmp}/sccache-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
: >"$tmp/sccache.conf"
[[ $# -gt 0 ]] || set -- cargo test --locked --lib --bins --tests --no-fail-fast
cpus=${LETO_TEST_CPUS:-0-15,32-47}
exec bwrap --dev-bind / / --unshare-net --die-with-parent -- \
  env -u RUSTC_WRAPPER -u CARGO_BUILD_RUSTC_WRAPPER \
  SCCACHE_DIR="$tmp/cache" SCCACHE_CONF="$tmp/sccache.conf" \
  SCCACHE_SERVER_PORT=4240 SCCACHE_IDLE_TIMEOUT=60 \
  nice -n 10 taskset -c "$cpus" "$@"
