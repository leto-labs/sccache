#!/usr/bin/env bash
# Runs a heavy build command (cargo build/clippy/install) niced, without a
# rustc wrapper, off the CPUs reserved on devbox. Tests go through
# leto/test-sandboxed.sh instead.
#
# Usage: leto/heavy.sh cargo build --release --locked
set -euo pipefail
exec env -u RUSTC_WRAPPER -u CARGO_BUILD_RUSTC_WRAPPER \
  nice -n 10 taskset -c "${LETO_TEST_CPUS:-0-15,32-47}" "$@"
