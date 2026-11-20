# Pre-warm the shared Rust cache

`prewarm.py` builds the canonical checkout's local branch tip through the
project's `scripts/cargo.sh`. It fetches from that checkout into a dedicated
shared clone with a detached HEAD. The canonical checkout is read-only. Each
project has a lock, a last-successful commit marker, a log, and a status file
under `~/.cache/leto-maintenance/prewarm/<project>/`. Failed or deferred builds
are retried on the next trigger. The runner records a commit only after every
configured Cargo command succeeds.

## Installation for Core

Use Linux with Git, GNU coreutils, util-linux, Python 3.11 or newer, the Leto
sccache client, and the project's usual Rust toolchain. Use a running shared
sccache server. The client selected for stats must connect to that server and
report `effective_cache_mode` in JSON. The default client port is 4226.

From this fork's checkout, run as the build user:

```sh
install -Dm755 leto/prewarm/prewarm.py ~/.local/bin/leto-prewarm
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
wrapper and the stats client. The config and cache roots honor
`XDG_CONFIG_HOME` and `XDG_CACHE_HOME`.

| Setting | Default and purpose |
| --- | --- |
| `PREWARM_REPO` | Required. Canonical Git checkout. |
| `PREWARM_BRANCH` | `main`. Local branch to warm. |
| `PREWARM_COMMANDS` | `check --workspace --all-targets;test --workspace --no-run`. Semicolon-separated Cargo argument lists, beginning with a subcommand that accepts `-j`. The runner supplies `-j`. |
| `PREWARM_JOBS` | A quarter of the CPUs in the runner's affinity, rounded down with a minimum of one. |
| `PREWARM_LOAD_LIMIT` | Half the CPUs in the runner's affinity. Uses the host's one-minute load average. |
| `PREWARM_POLL_SECONDS` | `5`. Interval between samples while Cargo runs. |
| `PREWARM_MAX_PAUSE_SECONDS` | `900`. Total pause allowance per attempt, in seconds. |
| `PREWARM_LOAD_CMD` | Optional test or host override. Bash command printing one load number. |
| `PREWARM_SCCACHE_BIN` | `sccache` on the configured PATH. Use the project's client binary for stats if the wrapper selects its own. |

Match the lane's toolchain, client settings, Rust flags, and Cargo profiles.
The runner leaves Rust flags, incremental settings, and build paths to Cargo
and the project wrapper. Keep build paths local to the checkout. If the
checkout's `.cargo/config.toml` has an `[env]` entry for `SCCACHE_BASEDIRS`,
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
`IOSchedulingClass=idle`. It defers a build above the load limit, stops the
active Cargo process group when load rises above it, and continues the group
when load returns to the limit or below. When the pause allowance expires it
kills the build group and leaves that tip eligible for retry. The shared
sccache daemon is outside the Cargo process group. Work executed by the
daemon itself can continue at the daemon's priority. With lane client-side
compilation, compiler children inherit the runner's priority and pause.

The wrapper still applies the project's benchmark controls. For a timing
run that requires an idle host or fixed cache contents, stop the project's
timer and service first, then start the timer again when the run is finished.

## Logs and disk cost

`prewarm.log` records the tip, build output, timing, disk use, and deltas for
compile requests, hits, misses, and writes. These are shared-server counters,
so concurrent lanes also contribute to them. `status.json` records the latest
attempt, the last successfully warmed commit, timings, and disk use. Check
both files to confirm a merge warmed, then compare hits and build time for
the same commands in a fresh lane. Existing incremental sessions can bypass
the cache or skip cache writes, and some crate types are uncacheable. The
checkout keeps its build output by default to speed subsequent warm-ups.

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
