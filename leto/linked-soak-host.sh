#!/usr/bin/env bash
# Soak of the linked-output stages on one project and host: two clones of the
# repo's HEAD, a private sccache server with rust_cache_linked = "all", clone A
# fills the cache, then leto/linked-soak.sh restores and rebuilds the same
# targets in clone B and compares their outputs.
#
# Usage: leto/linked-soak-host.sh REPO WORKDIR PORT SCCACHE_BIN [OPTIONS]
#   --targets tests|all  compare test harnesses (default) or also bins, examples
#                        and build scripts. --all is shorthand for --targets all.
#   --run      also run the restored test harnesses (needs the project's test env)
#   --run-bins "BIN [ARGS]"  run a restored bin with --all (repeatable)
#   --reflink  SCCACHE_DISK_REFLINK=true (btrfs hosts)
#   --remap    SCCACHE_BASEDIRS_REMAP_PATHS=1: require raw byte equality
# The shared server is never used: the run has its own port, config and cache.
set -euo pipefail
repo=$(readlink -f "${1:?repo}"); work=${2:?workdir}; port=${3:?port}; bin=$(readlink -f "${4:?sccache binary}")
shift 4
soak_args=() targets=tests reflink=false remap=
while [[ $# -gt 0 ]]; do
  case "$1" in
    --targets) targets=${2:?--targets needs tests or all}; shift ;;
    --all) targets=all ;;
    --run) soak_args+=(--run) ;;
    --run-bins) soak_args+=(--run-bins "${2:?--run-bins needs a command}"); shift ;;
    --reflink) reflink=true ;;
    --remap) remap=1 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
case "$targets" in tests | all) ;; *) echo "unknown target mode $targets" >&2; exit 2 ;; esac
case "$port" in 4226 | 4227) echo "port $port is reserved" >&2; exit 2 ;; esac
if ss -Hltn "sport = :$port" | grep -q .; then echo "port $port is in use" >&2; exit 2; fi
here=$(dirname "$(readlink -f "$0")")
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"

mkdir -p "$work"
work=$(readlink -f "$work")
[[ "$targets" != all ]] || soak_args+=(--warm-checkout "$work/a")
commit=$(git -C "$repo" rev-parse HEAD)
for c in a b; do
  [[ -e "$work/$c/.git" ]] || git clone -q --shared --no-checkout "$repo" "$work/$c"
  git -C "$work/$c" checkout -q --detach "$commit"
  git -C "$work/$c" submodule update -q --init --recursive 2>/dev/null || true
done
printf 'rust_cache_linked = "all"\n[cache.disk]\ndir = "%s/cache"\nsize = 214748364800\n' "$work" >"$work/sccache.conf"

while IFS='=' read -r name _; do [[ "$name" == SCCACHE_* ]] && unset "$name"; done < <(env)
unset CARGO_INCREMENTAL CARGO_BUILD_RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER CARGO_TARGET_DIR
export SCCACHE_CONF="$work/sccache.conf" SCCACHE_SERVER_PORT="$port" SCCACHE_IDLE_TIMEOUT=0
export SCCACHE_RUST_INCREMENTAL=cache SCCACHE_DISK_REFLINK="$reflink"
export SCCACHE_BASEDIRS_VERBATIM_ENV_DEPS=CARGO_MANIFEST_DIR,CARGO_MANIFEST_PATH
export SCCACHE_MISS_EXPLAIN="$work/misses.jsonl"
[[ -z "$remap" ]] || export SCCACHE_BASEDIRS_REMAP_PATHS=1
PATH="$(dirname "$bin"):$PATH"
export PATH RUSTC_WRAPPER="$bin" LONGBOW_SCCACHE_BIN="$bin"

"$bin" --stop-server >/dev/null 2>&1 || true
"$bin" --start-server >/dev/null
trap '"$bin" --stop-server >/dev/null 2>&1 || true' EXIT
echo "soak: commit $commit, server $("$bin" --version) on port $port"

t0=$(date +%s)
if ! (
  cd "$work/a" || exit
  export SCCACHE_BASEDIRS="$work/a"
  if [[ "$targets" == all ]]; then
    cargo build --workspace --bins --examples --message-format=json || exit
  fi
  cargo test --workspace --no-run --message-format=json
) >"$work/a.log" 2>&1; then
  echo "soak: clone A build failed, see $work/a.log" >&2
  exit 1
fi
echo "soak: clone A filled the cache in $(( $(date +%s) - t0 )) s"
"$bin" --show-stats --stats-format=json >"$work/stats-a.json"
"$bin" --zero-stats >/dev/null

soak_rc=0
SCCACHE_BASEDIRS="$work/b" "$here/linked-soak.sh" "$work/b" --targets "$targets" \
  "${soak_args[@]}" 2>"$work/b-soak.err" | tee "$work/b-soak.txt" || soak_rc=$?
"$bin" --show-stats --stats-format=json >"$work/stats-b.json"
python3 -I - "$work" <<'PY'
import json, sys
w = sys.argv[1]
for name in ("a", "b"):
    s = json.load(open(f"{w}/stats-{name}.json"))
    s = s.get("stats", s)
    print(f"stats {name}: requests {s['compile_requests']} hits {s['cache_hits'].get('counts')} "
          f"misses {s['cache_misses'].get('counts')} not cacheable {s['requests_not_cacheable']} "
          f"too large {s.get('rust_linked_entry_too_large', 0)}")
PY
echo "soak: linked-soak.sh exit $soak_rc"
exit "$soak_rc"
