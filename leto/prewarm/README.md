# Pre-warm the shared Rust cache

`prewarm.py` builds the canonical checkout's local branch tip through the
project's `scripts/cargo.sh`. It fetches from that checkout into a dedicated
shared clone with a detached HEAD. The canonical checkout is read-only. Each
project has a lock, a last-successful commit marker, a log, and a status file
under `~/.cache/leto-maintenance/prewarm/<project>/`. A failed tip backs off
before retrying. A deferred build remains eligible on the next trigger. The
runner records a commit only after every configured Cargo command succeeds.
See [the proposed fleet settings](proposed-confs.md) for project command lists
and their sizing estimates.

## Installation for Core

Use Linux with cgroup v2 and systemd 253 or newer, Git, GNU coreutils, util-linux,
Python 3.11 or newer, a Leto sccache client that supports
`SCCACHE_CLIENT_SIDE=required`, and the project's usual Rust toolchain. Use a
running shared sccache server. The stats client must connect to that server
and report `effective_cache_mode` in JSON. The default client port is 4226.

From this fork's checkout, run as the build user:

```sh
install -Dm755 leto/prewarm/prewarm.py ~/.local/bin/leto-prewarm
install -Dm755 leto/bounded.sh ~/.local/bin/leto-bounded
install -Dm644 leto/prewarm/systemd/leto-prewarm@.service ~/.config/systemd/user/leto-prewarm@.service
install -Dm644 leto/prewarm/systemd/leto-prewarm@.timer ~/.config/systemd/user/leto-prewarm@.timer
mkdir -p ~/.config/leto-prewarm
```

Create `~/.config/leto-prewarm/fairfill.conf` with your host's lane settings.
For example:

```sh
PREWARM_REPO=~/workspaces/leto-labs/fairfill
FAIRFILL_BUILD_SLOTS=0
SCCACHE_SERVER_PORT=4226
```

On devbox, put `CPUAffinity=0-15 32-47` under `[Service]` in a user-service
drop-in for `leto-prewarm@.service`. Keep the project's wrapper CPU settings
aligned with that partition. Use `taskset -c 0-15,32-47` for manual devbox runs.
Core can then load and enable the timer:

```sh
systemctl --user daemon-reload
systemctl --user enable --now leto-prewarm@fairfill.timer
```

Repeat the config and timer enablement for each project. The service PATH
includes `~/.local/bin` and `~/.cargo/bin`. Use `PATH` in the project config
when the lane uses a different toolchain or client location.

The timer is the recommended trigger. It checks every ten minutes with up to
90 seconds of jitter. It catches loose and packed refs, and it retries after a
busy host or a failed build without requiring another merge. The normal merge
workflow must advance the canonical **local** branch on each host. The timer
reads that ref and the runner fetches only from the canonical checkout.

## Config reference

The config uses one `NAME=value` per line. Quote values containing spaces or
`#`. Blank lines, comments, and an optional `export ` prefix are supported.
Values are literal after shell-style quote removal. `PREWARM_REPO` supports
`~` expansion. Other settings can use absolute paths. Project settings override
the inherited environment. Keys without the `PREWARM_` prefix go to the Cargo
wrapper and the stats client. The runner sets `SCCACHE_CLIENT_SIDE=required`
and `SCCACHE_SERVER_AUTOSTART=false` for warming. The config and cache roots honor
`XDG_CONFIG_HOME` and `XDG_CACHE_HOME`.

| Setting | Default and purpose |
| --- | --- |
| `PREWARM_REPO` | Required. Canonical Git checkout. |
| `PREWARM_BRANCH` | `main`. Local branch to warm. |
| `PREWARM_COMMANDS` | `check --workspace --all-targets;test --workspace --no-run`. Semicolon-separated Cargo argument lists in priority order, highest first. Each begins with a subcommand that accepts `-j`, which the runner supplies. |
| `PREWARM_COMMAND_TIMEOUTS` | `900`. Wall seconds per command, including pauses. Use one positive value for every command or a semicolon-separated value for each entry in `PREWARM_COMMANDS`. |
| `PREWARM_BACKOFF_MAX_MINUTES` | `120`. Maximum retry delay for consecutive failures at the same tip. The delay starts at seven minutes and doubles after each failure. |
| `PREWARM_TARGET_MAX_GIB` | `200`. Combined allocated GiB limit for Cargo's target and build directories after a successful warm. Set to `0` to disable automatic cleanup. |
| `PREWARM_MEMORY_MAX` | `8G`. Hard cap on each Cargo command and its descendants, with swap disabled. Accepts positive bytes or a size with a systemd suffix (`K`, `M`, `G`, `T`). |
| `PREWARM_JOBS` | A quarter of the CPUs in the runner's affinity, rounded down with a minimum of one. |
| `PREWARM_MIN_SPARE_CPUS` | `8`. Minimum host-wide spare logical CPUs after accounting for the runner's own usage. |
| `PREWARM_PAUSE_AFTER` | `60`. Seconds of sustained CPU shortage before pausing Cargo. |
| `PREWARM_RESUME_AFTER` | `30`. Seconds of sustained spare CPU while paused before Cargo can resume. |
| `PREWARM_MIN_MEM_AVAILABLE` | `0.15`. Minimum fraction of host memory available. Resuming requires five percentage points more. |
| `PREWARM_MAX_CPU_PRESSURE` | `80`. Maximum CPU pressure `some avg60` percentage. Set to `0` to disable this gate. |
| `PREWARM_POLL_SECONDS` | `5`. CPU sample interval before starting Cargo and while it runs. |
| `PREWARM_MAX_PAUSE_SECONDS` | `900`. Total pause allowance per attempt, in seconds. |
| `PREWARM_PAUSE_LOCKS` | Unset. Semicolon-separated lock files (`~` and `$VAR` expand; `${VAR:-default}` does not). While anyone holds one with an exclusive `flock` (FairFill's `scripts/bench-quiet.sh` holds `~/.cache/fairfill-bench/measurement.lock` during timed runs; Longbow's quiet host uses `~/.cache/longbow/locks/quiet-host.lock`), pre-warm waits before each command and pauses a running one (SIGSTOP), checking every second, then resumes as soon as the locks are free. The wait counts against `PREWARM_MAX_PAUSE_SECONDS`. A missing lock file counts as free. |
| `PREWARM_COMPRESSION` | `zstd:1`. btrfs compression property set on the pre-warm folder and, once, on its existing directories. sccache clones pre-warm outputs into the cache and a clone keeps the source's compression, so an uncompressed tree inflates the cache on disk. Empty disables it. Ignored on other filesystems. |
| `PREWARM_PROC_ROOT`, `PREWARM_CGROUP_ROOT` | Test overrides for `/proc` and `/sys/fs/cgroup`. |
| `PREWARM_SCCACHE_BIN` | `sccache` on the configured PATH. Use the project's client binary for stats if the wrapper selects its own. |

Match the lane's toolchain, client settings, Rust flags, and Cargo profiles.
The runner leaves Rust flags, incremental settings, and build paths to Cargo
and the project wrapper. Set checkout-local build paths in Cargo config or the
wrapper's environment so its `cargo metadata` reports the paths used to build.
If the checkout's `.cargo/config.toml` has an `[env]` entry for `SCCACHE_BASEDIRS`,
Cargo applies it. Otherwise the runner exports the dedicated checkout's path.
For a project such as Longbow, put
`LONGBOW_SCCACHE_BIN` and other lane-specific environment in its `.conf`.
Point `PREWARM_SCCACHE_BIN` at that same client. The wrapper applies the
checkout-relative remap and quiet-host policy.

## Lanes and timing benchmarks

Pre-warm clears `CARGO_MAKEFLAGS`, `MAKEFLAGS`, and `MFLAGS` and lets Cargo
create a private jobserver using the small `-j` limit. A process at idle
priority can wait a long time for CPU, and a stopped process can hold a token
for the entire pause. With private jobs, both cases leave the host FIFO's
tokens available to lanes. Disable project-wide build-slot admission in the
project config where appropriate so pre-warm also leaves those slots free.

The runner sets niceness to 19 (the usual `nice -n 19` for a manual run) and
applies `ionice -c3`. The service also sets `CPUWeight=idle`, `Nice=19`, and
`IOSchedulingClass=idle`. Each Cargo command inherits those process priorities
and runs in a transient user scope with `CPUWeight=idle`, `MemoryMax` and
`MemorySwapMax=0`. The runner uses `leto/bounded.sh` to launch these scopes and
manages their lifecycle so it can capture systemd's OOM result and write it
to status after the scope exits. The supervisor is outside the command
scope and stops it to terminate any remaining descendants. The scope also has a
systemd runtime limit as a fallback. Lane work keeps scheduling priority.

- The one-minute load average on devbox (64 logical CPUs) is often around 90,
  and on worker-02 (96 logical CPUs) it is often around 170. Load average
  counts runnable tasks and tasks in uninterruptible sleep. A threshold at
  half the CPU count can defer pre-warm indefinitely. The runner samples the
  host-wide CPU counters in `/proc/stat` and treats idle and I/O wait as
  unused CPU. Between commands it subtracts its cgroup's CPU usage, read from
  `cpu.stat` via `/proc/self/cgroup`. While building or when cgroup usage is
  unavailable, it sums CPU time from the process tree, which includes the
  command scope. This keeps pre-warm's own CPU use from
  causing a pause.
- The runner defers a new build below eight spare CPUs. During a build it
  stops the Cargo process group after 60 seconds below the threshold and
  requires 30 seconds at or above it before resuming.
- The runner also defers or pauses when `MemAvailable / MemTotal` drops below
  15%. After a pause, more than 20% must be available to resume.
- When `/proc/pressure/cpu` is available, the runner defers or pauses above
  80% CPU pressure (`some avg60`). The idle-weight pre-warm cgroup's tasks
  can themselves contribute to CPU pressure while waiting for lanes. Set
  `PREWARM_MAX_CPU_PRESSURE=0` if this repeatedly stalls pre-warm despite
  sufficient spare CPU and memory.

Start with eight spare CPUs on both hosts. This is one eighth of devbox and
one twelfth of worker-02. Set `PREWARM_MIN_SPARE_CPUS` in the host's project
config to reflect how much CPU the build needs to make useful progress.
On devbox, account for the pinned CPU partition when interpreting the
host-wide totals. Adjust `PREWARM_JOBS` for the runner's affinity on either
host, and raise the memory threshold if lane builds need more headroom.
The gate runs before each command. When the host becomes busy between
commands, the runner defers the rest of the attempt. The pause allowance
applies across all commands in an attempt. Running out of pause allowance
defers the attempt. A command timeout or scope OOM counts as a failure.
A new tip or a successful warm clears the previous failure streak.

Pre-warm requires client-side compilation. Cargo's sccache clients run the
compiler as their own child process and use the shared daemon as a cache
gateway. This keeps compiler work inside Cargo's scope, where the idle
settings, memory cap and resource gate apply. Both modes use the same
cache-key pipeline.
[Client-side compilation architecture](../../docs/Architecture.md#client-side-mode-sccache_client_side)

On btrfs, a compatible client and daemon can use a same-user Unix socket to
reflink compatible output files into the cache. See
[reflink cache hits](../../docs/Local.md#reflink-cache-hits).

Keep `SCCACHE_LOG` unset and leave distributed compilation disabled for
pre-warm. The required client mode rejects these settings. When the server has
miss diagnostics enabled, pre-warm requests still use the cache and skip
server-side miss logging.

The wrapper still applies the project's benchmark controls. For a timing
run that requires an idle host or fixed cache contents, stop the project's
timer and service first, then start the timer again when the run is finished.

## Logs and disk cost

`prewarm.log` records the tip, build output, timing, disk use, and deltas for
compile requests, hits, misses, and writes. These are shared-server counters,
so concurrent lanes also contribute to them. `status.json` records the latest
attempt, the last successfully warmed commit, per-command budgets and
results, timings, and disk use. It records `oom` for a scope OOM kill and
`timed_out` for a command that runs out of time. For a failed tip, it keeps
the failure count, next retry time, and last error across triggers. The error
includes the command and the last non-empty line from its 64 KiB stderr tail,
or the runner's failure message when stderr is empty. Check
both files to confirm a merge warmed, then compare hits and build time for
the same commands in a fresh lane. Existing incremental sessions can bypass
the cache or skip cache writes, and some crate types are uncacheable. The
checkout keeps its build output below the cap to speed subsequent warm-ups.

After every successful warm with a cap enabled, an idle-priority disk scan
measures Cargo's output directories with a 30-second budget. Going over the
cap or timing out triggers a full clean. This removes Cargo's fingerprints
along with their artifacts and keeps even a single oversized build from
accumulating output. The next tip can restore cacheable outputs from the
shared cache and rebuild the rest. The runner records allocated bytes before
and after, the paths removed, and any cleanup error under `target_prune` in
`status.json`.

Disk reports include apparent and allocated bytes for the whole clone and
break out `target/` from the rest of the checkout. Any other checkout-local
build directories are included in the total. On btrfs, the report also includes
`compsize -b` output if it works directly or through `sudo -n`. Reflinks can
share extents with the cache or other checkouts, so allocated bytes are an
estimate of the space this checkout accounts for. A lock contender leaves the
active run's status file alone.

Budget for one source clone plus one target tree per enabled project on each
host. Prior measurements after the default `check` and `test --no-run` builds
put FairFill on devbox (ext4) at about **40–42 GB** for `target/`, including
about **16 GB** of incremental state. Longbow on worker-01 (btrfs with zstd:1
and rust-lld) measured about **48–50 GB apparent** and **17 GB on disk** for
`target/`. With `leto-0.19.0-6`, a first build creates no incremental sessions,
so most of the incremental portion disappears from those estimates for the
first build. A smaller option is `PREWARM_COMMANDS='check --workspace --all-targets'`,
which skips the test build.

To reclaim the output, run `~/.local/bin/leto-prewarm <project> --clean`
with the host CPU restrictions if applicable. It takes the same lock, looks
up Cargo's output directories through the wrapper, and removes those within
the dedicated checkout. The last-warmed marker and shared cache are retained.
