#!/usr/bin/env bash
# Runs a command (default: the CI test set) in a private network namespace.
# The test run has its own loopback, so it cannot reach a shared server.
# The harness restores the sandbox's port after stripping SCCACHE_* variables.
#
# bwrap where unprivileged user namespaces work (devbox); otherwise sudo
# unshare -n, then back to this user with setpriv (the workers restrict
# unprivileged user namespaces).
#
# Usage: leto/test-sandboxed.sh [command ...]
#   leto/test-sandboxed.sh cargo test --locked --test system -- basedirs
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")/.."
# Under $HOME, not /tmp: tests that need btrfs reflinks probe their temp dir
# and skip on other filesystems (on the workers /tmp is ext4). Not inside the
# repo either: fixture crates there would find this repo's [workspace].
tmp_root=${LETO_TEST_TMP:-${XDG_CACHE_HOME:-$HOME/.cache}/leto-sccache-test-tmp}
mkdir -p "$tmp_root"
tmp=$(mktemp -d "$tmp_root/sccache-test.XXXXXX")
export TMPDIR="$tmp"
trap 'rm -rf "$tmp"' EXIT
: >"$tmp/sccache.conf"
[[ $# -gt 0 ]] || set -- cargo test --locked --lib --bins --tests --no-fail-fast
cpus=${LETO_TEST_CPUS:-0-15,32-47}
port=4240

if bwrap --dev-bind / / --unshare-net -- true 2>/dev/null; then
  sandbox=(bwrap --dev-bind / / --unshare-net --die-with-parent --)
elif sudo -n true 2>/dev/null; then
  export -p >"$tmp/env"
  sandbox=(sudo -n unshare -n -- bash -c '
    ip link set lo up &&
    exec setpriv --reuid="$1" --regid="$2" --init-groups -- \
      bash -c ". \"\$0\" && exec \"\$@\"" "$3" "${@:4}"' sh "$(id -u)" "$(id -g)" "$tmp/env")
else
  echo "test-sandboxed: no way to get a private network namespace here" >&2
  exit 2
fi

"${sandbox[@]}" \
  env -u RUSTC_WRAPPER -u CARGO_BUILD_RUSTC_WRAPPER \
  SCCACHE_DIR="$tmp/cache" SCCACHE_CONF="$tmp/sccache.conf" \
  SCCACHE_SERVER_PORT="$port" LETO_TEST_SERVER_PORT="$port" SCCACHE_IDLE_TIMEOUT=60 \
  nice -n 10 taskset -c "$cpus" "$@"
