#!/usr/bin/env bash
set -euo pipefail

runner=$(realpath -- "$(dirname -- "$0")/prewarm.py")
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
tmp=$(mktemp -d)
prewarm_pid=
sampler_pid=

cleanup() {
  if [[ -n "$prewarm_pid" ]]; then
    kill "$prewarm_pid" 2>/dev/null || true
    wait "$prewarm_pid" 2>/dev/null || true
  fi
  if [[ -n "$sampler_pid" ]]; then
    kill "$sampler_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true
  fi
  rm -rf -- "$tmp"
}
trap cleanup EXIT

unset "${!CARGO_@}" "${!GIT_@}" "${!PREWARM_@}" "${!SCCACHE_@}"
unset RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER RUSTFLAGS MAKEFLAGS MFLAGS
export HOME="$tmp/home" RUSTUP_HOME="$rustup_home" CARGO_HOME="$tmp/cargo" CARGO_NET_OFFLINE=true
export XDG_CACHE_HOME="$HOME/.cache" XDG_CONFIG_HOME="$HOME/.config"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export SCCACHE_SERVER_PORT=4250 SCCACHE_CONF="$tmp/sccache.conf" SCCACHE_DIR="$tmp/sccache-cache"
export PATH="$tmp/bin:/usr/bin:$PATH" FIXTURE_CONTROL="$tmp/control"
export PREWARM_PROC_ROOT="$tmp/proc" PREWARM_CGROUP_ROOT="$tmp/cgroup"
export PREWARM_SCCACHE_BIN="$tmp/bin/sccache"
unset FIXTURE_EXTRA

repo="$tmp/main-repo"
state="$XDG_CACHE_HOME/leto-maintenance/prewarm/fixture"
checkout="$state/checkout"
config="$XDG_CONFIG_HOME/leto-prewarm/fixture.conf"
log="$state/prewarm.log"
mkdir -p -- "$FIXTURE_CONTROL" "$tmp/bin" "$repo/scripts" "$repo/crate/src" "$repo/.cargo" "$(dirname -- "$config")" \
  "$PREWARM_PROC_ROOT/self/task/1" "$PREWARM_PROC_ROOT/pressure" "$PREWARM_PROC_ROOT/100" "$PREWARM_CGROUP_ROOT/fixture"
: >"$SCCACHE_CONF"
: >"$FIXTURE_CONTROL/cargo.log"
: >"$FIXTURE_CONTROL/sccache.log"
printf '0 0\n' >"$FIXTURE_CONTROL/cpu"
printf '0::/fixture\n' >"$PREWARM_PROC_ROOT/self/cgroup"
printf '1 (prewarm) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n' >"$PREWARM_PROC_ROOT/self/stat"
printf '100\n' >"$PREWARM_PROC_ROOT/self/task/1/children"

set_cpu() {
  printf '%s %s\n' "$1" "$2" >"$FIXTURE_CONTROL/cpu.new"
  mv -- "$FIXTURE_CONTROL/cpu.new" "$FIXTURE_CONTROL/cpu"
}

set_memory() {
  printf 'MemTotal: 1000 kB\nMemAvailable: %s kB\n' "$1" >"$PREWARM_PROC_ROOT/meminfo.new"
  mv -- "$PREWARM_PROC_ROOT/meminfo.new" "$PREWARM_PROC_ROOT/meminfo"
}

set_pressure() {
  printf 'some avg10=99.00 avg60=%s avg300=0.00 total=10\n' "$1" >"$PREWARM_PROC_ROOT/pressure/cpu.new"
  mv -- "$PREWARM_PROC_ROOT/pressure/cpu.new" "$PREWARM_PROC_ROOT/pressure/cpu"
}

set_memory 500
python3 - <<'PY' &
import os
from pathlib import Path
import time

proc = Path(os.environ["PREWARM_PROC_ROOT"])
cgroup = Path(os.environ["PREWARM_CGROUP_ROOT"]) / "fixture"
control = Path(os.environ["FIXTURE_CONTROL"]) / "cpu"
clock_ticks = os.sysconf("SC_CLK_TCK")
cpus = 16

def write(path, contents):
    pending = path.with_suffix(".new")
    pending.write_text(contents)
    pending.replace(path)

started = previous = time.monotonic()
busy = own = 0
while True:
    busy_cpus, own_cpus = map(int, control.read_text().split())
    now = time.monotonic()
    busy += (now - previous) * busy_cpus
    own += (now - previous) * own_cpus
    previous = now
    idle_ticks = int(((now - started) * cpus - busy) * clock_ticks)
    write(cgroup / "cpu.stat", f"usage_usec {int(own * 1e6)}\n")
    ticks = int(own * clock_ticks)
    write(proc / "100/stat", f"100 (rustc) S 1 0 0 0 0 0 0 0 0 0 {ticks // 2} {ticks - ticks // 2} 0 0\n")
    write(proc / "stat", f"cpu {int(busy * clock_ticks)} 0 0 {idle_ticks // 2} {idle_ticks - idle_ticks // 2} 0 0 0 0 0\n"
          + "".join(f"cpu{i} 0 0 0 0 0 0 0 0 0 0\n" for i in range(cpus)))
    time.sleep(0.02)
PY
sampler_pid=$!

cat >"$tmp/bin/sccache" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$SCCACHE_SERVER_PORT" == 4250 ]]
if [[ "$*" == '--show-stats --stats-format=json' ]]; then
  count=$(wc -l <"$FIXTURE_CONTROL/sccache.log")
  printf '{"effective_cache_mode":"ReadWrite","stats":{"compile_requests":%s,"cache_hits":{"counts":{}},"cache_misses":{"counts":{"Rust":%s}},"cache_writes":%s}}\n' \
    "$count" "$count" "$count"
  exit
fi
[[ ${1##*/} == rustc ]]
printf '%s|%s\n' "${SCCACHE_CLIENT_SIDE:-}" "$*" >>"$FIXTURE_CONTROL/sccache.log"
if [[ -e "$FIXTURE_CONTROL/hold" && " $* " == *' --crate-name fixture '* ]]; then
  printf '%s\n' "$BASHPID" >"$FIXTURE_CONTROL/rustc.pid"
  while [[ -e "$FIXTURE_CONTROL/hold" ]]; do sleep 0.1; done
fi
exec "$@"
EOF
chmod +x "$tmp/bin/sccache"

cat >"$repo/scripts/cargo.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ ! -v CARGO_MAKEFLAGS && ! -v MAKEFLAGS && ! -v MFLAGS ]]
printf '%s|%s|%s|%s\n' "$*" "${FIXTURE_EXTRA:-}" "${SCCACHE_BASEDIRS:-}" "${SCCACHE_CLIENT_SIDE:-}" \
  >>"$FIXTURE_CONTROL/cargo.log"
export RUSTC_WRAPPER="${FIXTURE_CONTROL%/*}/bin/sccache"
exec cargo "$@"
EOF

cat >"$repo/Cargo.toml" <<'EOF'
[workspace]
members = ["crate"]
resolver = "3"
EOF
cat >"$repo/crate/Cargo.toml" <<'EOF'
[package]
name = "fixture"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"

[lints.rust]
unsafe_code = "warn"

[lints.clippy]
all = "warn"
pedantic = "warn"
EOF
printf 'pub fn value() -> u8 { 1 }\n' >"$repo/crate/src/lib.rs"
printf '/target/\n' >"$repo/.gitignore"

git -C "$repo" init -q -b main
git -C "$repo" config user.name Fixture
git -C "$repo" config user.email fixture@example.invalid
git -C "$repo" add .
git -C "$repo" commit -qm initial
tip=$(git -C "$repo" rev-parse HEAD)

cat >"$config" <<EOF
PREWARM_REPO="$repo"
PREWARM_JOBS=1
PREWARM_POLL_SECONDS=0.1
PREWARM_PAUSE_AFTER=0.5
PREWARM_RESUME_AFTER=0.4
PREWARM_MAX_PAUSE_SECONDS=10
FIXTURE_EXTRA='value from project config'
CARGO_MAKEFLAGS='-j --jobserver-auth=fifo:/nonexistent'
MAKEFLAGS='-j --jobserver-auth=fifo:/nonexistent'
MFLAGS='-j --jobserver-auth=fifo:/nonexistent'
EOF

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  if [[ -f "$log" ]]; then cat "$log" >&2; fi
  exit 1
}

run() {
  "$runner" fixture "$@" >"$tmp/run.log" || fail "prewarm command failed"
}

status_is() {
  python3 - "$state/status.json" "$1" "$2" <<'PY'
import json
import sys
from pathlib import Path

status = json.loads(Path(sys.argv[1]).read_text())
assert status["result"] == sys.argv[2], status
assert status["last_warmed_commit"] == (sys.argv[3] or None), status
for key in ("apparent_bytes", "allocated_bytes"):
    disk = status["disk"]
    assert disk["checkout"][key] == disk["source"][key] + disk["target"][key], disk
    if sys.argv[2] == "warmed":
        assert disk["target"][key] > 0, disk
    if sys.argv[2] == "cleaned":
        assert disk["target"][key] == 0, disk
assert status["duration_seconds"] >= status["build_seconds"] >= status["paused_seconds"], status
PY
}

change_tip() {
  printf 'pub fn value() -> u8 { %s }\n' "$1" >"$repo/crate/src/lib.rs"
  git -C "$repo" add .
  git -C "$repo" commit -qm "value $1"
  tip=$(git -C "$repo" rev-parse HEAD)
}

wait_for() {
  local end=$((SECONDS + 15))
  until "$@"; do
    ((SECONDS < end)) || fail "timed out waiting for $*"
    sleep 0.05
  done
}

settled() {
  wait "$prewarm_pid" || fail "background prewarm failed"
  prewarm_pid=
}

process_stopped() {
  ps -o state= -p "$1" | grep -qx T
}

resume_count_is() {
  [[ $(grep -c 'resumed pid=' "$tmp/background.log") == "$1" ]]
}

wait_for test -s "$PREWARM_PROC_ROOT/stat"
SCCACHE_CLIENT_SIDE=0 run
status_is warmed "$tip"
[[ $(wc -l <"$FIXTURE_CONTROL/cargo.log") == 2 ]] || fail "expected both Cargo commands"
grep -Fxq "check --workspace --all-targets -j 1|value from project config|$checkout|1" \
  "$FIXTURE_CONTROL/cargo.log" || fail "expected project env, checkout-based normalization and client-side mode"
grep -Fxq "test --workspace --no-run -j 1|value from project config|$checkout|1" \
  "$FIXTURE_CONTROL/cargo.log" || fail "missing default test build"
grep -Eq '^1[|].*--crate-name fixture' "$FIXTURE_CONTROL/sccache.log" || fail "sccache did not get client-side mode"
grep -Eq 'shared sccache delta \{"compile_requests": [1-9]' "$log" || fail "missing stats delta"
[[ ! -e "$repo/target" ]] || fail "canonical repo received build output"
printf 'ok - spare CPU starts a client-side build with project env, private jobs, stats and disk status\n'

run
status_is unchanged "$tip"
[[ $(wc -l <"$FIXTURE_CONTROL/cargo.log") == 2 ]] || fail "unchanged tip rebuilt"
printf 'ok - unchanged tip skips Cargo\n'

printf '[env.SCCACHE_BASEDIRS]\nvalue = ""\nrelative = true\n' >"$repo/.cargo/config.toml"
change_tip 2
git -C "$repo" pack-refs --all
printf 'SCCACHE_CLIENT_SIDE=0\n' >>"$config"
SCCACHE_CLIENT_SIDE=1 run
status_is warmed "$tip"
[[ $(git -C "$checkout" rev-parse HEAD) == "$tip" ]] || fail "checkout did not advance"
[[ $(wc -l <"$FIXTURE_CONTROL/cargo.log") == 4 ]] || fail "new tip did not build"
tail -n 2 "$FIXTURE_CONTROL/cargo.log" | grep -Fxq 'check --workspace --all-targets -j 1|value from project config||0' \
  || fail "Cargo did not get the client-side override or its configured basedirs"
grep -Eq '^0[|].*--crate-name fixture' "$FIXTURE_CONTROL/sccache.log" || fail "sccache did not get the client-side override"
sed -i '/^SCCACHE_CLIENT_SIDE=/d' "$config"
printf 'ok - new packed branch tip builds and respects Cargo config and the client-side override\n'

last_warmed=$tip
change_tip 3
set_cpu 16 0
run
status_is deferred "$last_warmed"
[[ $(wc -l <"$FIXTURE_CONTROL/cargo.log") == 4 ]] || fail "started Cargo without spare CPU"
[[ $(git -C "$checkout" rev-parse HEAD) == "$last_warmed" ]] || fail "fetched without spare CPU"
grep -q 'deferred .*: spare CPU' "$tmp/run.log" || fail "missing CPU deferral reason"
printf 'ok - a busy host with no spare CPU defers an unwarmed tip\n'

set_cpu 0 0
touch "$FIXTURE_CONTROL/hold"
"$runner" fixture >"$tmp/background.log" &
prewarm_pid=$!
wait_for test -s "$FIXTURE_CONTROL/rustc.pid"
run
grep -q 'pre-warm already running' "$tmp/run.log" || fail "lock allowed overlapping builds"
status_is deferred "$last_warmed"
printf 'ok - an active build holds the project lock\n'

set_cpu 16 0
wait_for grep -q 'paused pid=.*spare CPU' "$tmp/background.log"
rustc_pid=$(cat "$FIXTURE_CONTROL/rustc.pid")
read -r cargo_pid < <(ps -o pgid= -p "$rustc_pid")
for pid in "$cargo_pid" "$rustc_pid"; do
  wait_for process_stopped "$pid"
done
printf 'ok - sustained CPU shortage stops the Cargo group\n'
set_cpu 0 0
wait_for grep -q 'resumed pid=' "$tmp/background.log"
printf 'ok - sustained CPU recovery resumes the Cargo group\n'

set_memory 100
wait_for grep -q 'paused pid=.*available memory' "$tmp/background.log"
wait_for process_stopped "$rustc_pid"
set_memory 175
sleep 0.7
process_stopped "$rustc_pid" || fail "resumed before memory reached its recovery margin"
set_memory 250
wait_for resume_count_is 2
printf 'ok - low memory pauses Cargo until available memory clears the recovery margin\n'

set_pressure 90
wait_for grep -q 'paused pid=.*CPU pressure avg60' "$tmp/background.log"
wait_for process_stopped "$rustc_pid"
set_pressure 20
wait_for resume_count_is 3
printf 'ok - enabled CPU pressure gate pauses Cargo until avg60 drops\n'

rm -- "$FIXTURE_CONTROL/hold"
settled
status_is warmed "$tip"

last_warmed=$tip
change_tip 4
export PREWARM_COMMANDS='check --workspace --all-targets'
rm -- "$FIXTURE_CONTROL/rustc.pid"
touch "$FIXTURE_CONTROL/hold"
set_cpu 16 16
set_pressure 90
PREWARM_MAX_CPU_PRESSURE=0 "$runner" fixture >"$tmp/background.log" &
prewarm_pid=$!
wait_for test -s "$FIXTURE_CONTROL/rustc.pid"
sleep 0.8
if grep -q 'paused pid=' "$tmp/background.log"; then fail "own CPU or disabled pressure gate paused Cargo"; fi
rm -- "$FIXTURE_CONTROL/hold"
settled
status_is warmed "$tip"
printf 'ok - subtracting own cgroup CPU keeps Cargo running on a busy host with the pressure gate disabled\n'

change_tip 5
PREWARM_CGROUP_ROOT="$tmp/missing-cgroup" PREWARM_MAX_CPU_PRESSURE=0 run
status_is warmed "$tip"
printf 'ok - process-tree CPU accounting works without cgroup usage\n'

last_warmed=$tip
change_tip 6
set_cpu 0 0
set_pressure 20
sed -i 's/PREWARM_MAX_PAUSE_SECONDS=10/PREWARM_MAX_PAUSE_SECONDS=0.5/' "$config"
rm -- "$FIXTURE_CONTROL/rustc.pid"
touch "$FIXTURE_CONTROL/hold"
"$runner" fixture >"$tmp/background.log" &
prewarm_pid=$!
wait_for test -s "$FIXTURE_CONTROL/rustc.pid"
set_cpu 16 0
if wait "$prewarm_pid"; then fail "expired pause reported success"; fi
prewarm_pid=
status_is failed "$last_warmed"
grep -q 'maximum pause time reached' "$tmp/background.log" || fail "build did not time out"
rustc_pid=$(cat "$FIXTURE_CONTROL/rustc.pid")
if ps -o state= -p "$rustc_pid" | grep -Eq '^[^ZX]$'; then
  fail "pause timeout left the compiler running"
fi
rm -- "$FIXTURE_CONTROL/hold"
set_cpu 0 0
run
status_is warmed "$tip"
printf 'ok - pause timeout leaves the tip eligible for retry\n'

mkdir -p "$tmp/outside"
touch "$tmp/outside/keep"
if CARGO_TARGET_DIR="$tmp/outside" "$runner" fixture --clean >"$tmp/run.log"; then
  fail "clean accepted an external Cargo target"
fi
[[ -f "$tmp/outside/keep" && -d "$checkout/target" ]] || fail "clean removed output before validating paths"
run --clean
status_is cleaned "$tip"
[[ ! -e "$checkout/target" && ! -e "$repo/target" ]] || fail "clean left build output"
printf 'ok - clean reclaims checkout output, keeps the marker, and refuses external paths\n'

cat >"$tmp/bin/stat" <<'EOF'
#!/usr/bin/env bash
[[ "$*" == '-f -c %T .' ]] || exit 1
printf 'btrfs\n'
EOF
cat >"$tmp/bin/compsize" <<'EOF'
#!/usr/bin/env bash
[[ "$*" == '-b .' ]] || exit 2
printf 'direct\n' >>"$FIXTURE_CONTROL/compsize.log"
exit 1
EOF
cat >"$tmp/bin/sudo" <<'EOF'
#!/usr/bin/env bash
[[ "$*" == '-n compsize -b .' ]] || exit 2
printf 'sudo\n' >>"$FIXTURE_CONTROL/compsize.log"
printf 'fixture compressed bytes\n'
EOF
chmod +x "$tmp/bin/stat" "$tmp/bin/compsize" "$tmp/bin/sudo"
run
status_is unchanged "$tip"
[[ $(cat "$FIXTURE_CONTROL/compsize.log") == $'direct\nsudo' ]] || fail "wrong compsize invocation order"
grep -q '"compsize": "fixture compressed bytes"' "$state/status.json" || fail "missing stats in JSON"
grep -q '"compsize": "fixture compressed bytes"' "$log" || fail "missing stats in log"
printf 'ok - btrfs disk report falls back to noninteractive compsize and records its output\n'
