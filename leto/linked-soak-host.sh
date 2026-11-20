#!/usr/bin/env bash
# Soak of the linked-output stages on one project and host: two clones of the
# repo's HEAD, a private sccache server with rust_cache_linked = "all", clone A
# fills the cache with `cargo test --workspace --no-run`, then
# leto/linked-soak.sh restores clone B's harnesses from the cache, rebuilds them
# with the cache bypassed and compares the binaries.
#
# Usage: leto/linked-soak-host.sh REPO WORKDIR PORT SCCACHE_BIN [--run] [--reflink]
#   --run      also run the restored test harnesses (needs the project's test env)
#   --reflink  SCCACHE_DISK_REFLINK=true (btrfs hosts)
# The shared server is never used: the run has its own port, config and cache.
set -euo pipefail
repo=$(readlink -f "${1:?repo}"); work=${2:?workdir}; port=${3:?port}; bin=$(readlink -f "${4:?sccache binary}")
shift 4
soak_args=() reflink=false
for a in "$@"; do
  case "$a" in
    --run) soak_args+=(--run) ;;
    --reflink) reflink=true ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done
case "$port" in 4226 | 4227) echo "port $port is reserved" >&2; exit 2 ;; esac
if ss -Hltn "sport = :$port" | grep -q .; then echo "port $port is in use" >&2; exit 2; fi
here=$(dirname "$(readlink -f "$0")")
command -v cargo >/dev/null || PATH="$HOME/.cargo/bin:$PATH"

mkdir -p "$work"
work=$(readlink -f "$work")
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
PATH="$(dirname "$bin"):$PATH"
export PATH RUSTC_WRAPPER="$bin" LONGBOW_SCCACHE_BIN="$bin"

"$bin" --stop-server >/dev/null 2>&1 || true
"$bin" --start-server >/dev/null
trap '"$bin" --stop-server >/dev/null 2>&1 || true' EXIT
echo "soak: commit $commit, server $("$bin" --version) on port $port"

t0=$(date +%s)
(cd "$work/a" && SCCACHE_BASEDIRS="$work/a" cargo test --workspace --no-run -q) >"$work/a.log" 2>&1 \
  || { echo "soak: clone A build failed, see $work/a.log" >&2; exit 1; }
echo "soak: clone A filled the cache in $(( $(date +%s) - t0 )) s"
"$bin" --show-stats --stats-format=json >"$work/stats-a.json"
"$bin" --zero-stats >/dev/null

SCCACHE_BASEDIRS="$work/b" "$here/linked-soak.sh" "$work/b" "${soak_args[@]}" 2>"$work/b-soak.err" | tee "$work/b-soak.txt"
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
