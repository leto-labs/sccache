#!/usr/bin/env bash
# Runs a heavy build command (cargo build/clippy/install) on the build CPUs,
# queued on the host's build-bench timing lock so it never overlaps a timing
# run. Tests go through leto/test-sandboxed.sh instead.
#
# Usage: leto/heavy.sh cargo build --release --locked
set -euo pipefail
lock=$HOME/.cache/leto-maintenance/build-bench/timing.lock
mkdir -p "$(dirname "$lock")"
exec flock -o "$lock" env -u RUSTC_WRAPPER -u CARGO_BUILD_RUSTC_WRAPPER \
  nice -n 10 taskset -c "${LETO_TEST_CPUS:-0-15,32-47}" "$@"
