# Proposed pre-warm settings

Core can merge these lines into the existing project configs. Keep each
project's checkout, wrapper, toolchain, and shared-server settings. Install
the runner and scope helper, then point the stats client and project wrapper
at an sccache client supporting `SCCACHE_CLIENT_SIDE=required`. The existing
daemon protocol supports this mode. The next new main tip is the first one
eligible for the expanded command list.

## devbox

`~/.config/leto-prewarm/fairfill.conf`

```sh
FAIRFILL_BUILD_SLOTS=0
FAIRFILL_BUILD_CPUS=0-15,32-47
FAIRFILL_BUILD_CPUS_DURING_BENCH=0-7,32-39
PREWARM_JOBS=2
PREWARM_MEMORY_MAX=8G
PREWARM_COMMANDS='clippy --workspace --all-targets --all-features --locked -- -D warnings;build --profile fast --locked -p fairfill-search-backtest;build --profile fast --locked -p fairfill-search-types --example execution_cost;test --workspace --all-features --locked --lib --bins --tests --examples --no-run;build --release --locked -p fairfill-search-live --bin fairfill-search-live'
PREWARM_COMMAND_TIMEOUTS='600;1200;600;1500;1800'
```

Retain the user-service affinity `0-15 32-47`. The checked-in catalog and
script tests use the `fast` commands. The scorecard installer and paper
service use the `release` binary.

## worker-01

`~/.config/leto-prewarm/longbow.conf`

```sh
PREWARM_JOBS=2
PREWARM_MEMORY_MAX=8G
PREWARM_COMMANDS='build -p longbow-cli --bin longbow;test --workspace --no-run;clippy --workspace --all-targets --all-features'
PREWARM_COMMAND_TIMEOUTS='600;1500;600'
```

The recent Clippy failures stopped the binary and test commands from starting.
Run those first. Keep the existing Longbow client and private jobserver settings.

## worker-02

`~/.config/leto-prewarm/markets-rs.conf`

```sh
PREWARM_JOBS=2
PREWARM_MEMORY_MAX=8G
PREWARM_COMMANDS='clippy --workspace --all-targets --all-features --locked -- -D warnings;clippy --workspace --all-targets --locked -- -D warnings;test --workspace --exclude marketlab-python --all-features --locked --tests --examples --no-run'
PREWARM_COMMAND_TIMEOUTS='900;900;1800'
```

The short metadata passes cover both feature sets already used on this host.
Keep the workspace gate's test and example selection. Clear old checkout
outputs before adding another Markets-rs profile.

`~/.config/leto-prewarm/research-rag.conf`

```sh
PREWARM_JOBS=2
PREWARM_MEMORY_MAX=8G
PREWARM_COMMANDS='clippy --workspace --all-targets --all-features --locked -- -D warnings;test --workspace --locked --no-run;build --release --locked -p rr-cli --bin rr -p rr-service --bin research-rag-query'
PREWARM_COMMAND_TIMEOUTS='300;450;1200'
```

The release command matches the deploy script's two binaries.

## Observed new misses

Samples ended near 20:18 UTC on 2026-11-21. Memory PSI `some avg10` at
collection was 0.11% on devbox, 0.00% on worker-01, and 0.00% on worker-02.
The read windows were 63.94 MiB from 07:43 UTC on devbox, 48.37 MiB from
19:03 UTC the previous day on worker-01, and 64 MiB from 03:18 UTC on
worker-02. The devbox schema probe used the remaining 64 KiB. Each read
ran with `nice -n 19 ionice -c3 timeout 120`. Counts are miss events.
There were 722 unassigned new records across the hosts.

The log schema has `cwd`, `basedir` and artifact basenames. It omits rustc
arguments, output directories and feature values for `new` events. Rustc
usually runs from the crate source directory. Paths under the FairFill
registration fixtures and the Longbow scratch checkouts identify the custom
directories, with the actual profile still unknown. An executable artifact
can come from a harness, binary, example, bench or build script. These counts
are the observable upper pools for command selection and include units that
depend on lane-only source, features or paths. The samples also include
pre-warm and benchmark events carrying a project path.

| Project | Profile or working directory | Metadata | rlib | Shared library | Executable | Total new |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| FairFill | Unknown | 778 | 3,288 | 19 | 5,337 | 9,422 |
| FairFill | Unknown, `target/registration-fixture` | 652 | 22 | 0 | 55 | 729 |
| Longbow | Unknown | 2,548 | 6,681 | 482 | 802 | 10,513 |
| Longbow | Unknown, scratch directories under `target/` | 7 | 30 | 0 | 106 | 143 |
| Markets-rs | Unknown | 4,163 | 3,338 | 106 | 14,130 | 21,737 |
| research-rag | Unknown | 175 | 215 | 343 | 397 | 1,130 |

Most frequent workspace crates on the new records:

- FairFill: `fairfill_search_live` 356, `fairfill_search_backtest` 325,
  `fairfill_indexer` 233, `fairfill_search_preparation` 155.
- Longbow: `arrow_ipc` and `arrow` 103 each, `arrow_array` 100,
  `arrow_cmp` and `arrow_select` 99 each. Registry crates include
  `thiserror` 153 and `anyhow` 141.
- Markets-rs: `marketlab_backtest` 308, `marketlab_datasets` 270,
  `marketlab_research` 263, `integration` 239, `marketlab_service` 232.
- research-rag: `research_rag_query` 47, `integration` 34, `rr_query` 33,
  `rr_testkit` 21. Registry crates include `anyhow` 55,
  `schemars_derive` 51, `pin_project_internal` and `prost_derive` 50 each.

## Command selection and cost

The existing command times come from the pre-warm logs accompanying
`status.json`. They use 8 jobs on FairFill and Longbow and 24 on worker-02.
The Core build-bench reports put standard cold and reused checkout builds at
about 3–4 min for FairFill, 4–8 min for Longbow, and 7–18 min for Markets-rs
(with longer runs on loaded hosts). These are checks and test builds. The
named-profile times below are planning estimates for the initial builds at
the proposed lower job count. The configured budgets leave extra time for
idle scheduling and linking.

The rates are indeterminate from these fields. Short metadata passes
have the lowest observed wall cost. The named-profile choices
prioritize checked-in lane commands and frequently missed workspace crates.
Multiple commands can cover the same dependency units.

| Project | Profile and candidate command | Attributable new misses | Estimated wall cost | Chosen |
| --- | --- | --- | --- | --- |
| FairFill | dev, all-feature workspace Clippy | Unknown, metadata pool 778 outside the registration fixtures | 9–69 s observed | Yes |
| FairFill | fast, build backtest package | Unknown, backtest crate 325 across all profiles and kinds | 4–12 min | Yes |
| FairFill | fast, build `execution_cost` example | Unknown | 1–5 min after the fast build, if dependencies overlap | Yes |
| FairFill | test, all-feature workspace tests and examples | Unknown, executable pool 5,337 outside the registration fixtures | 1–9.5 min observed | Yes |
| FairFill | release, build live binary | Unknown, live crate 356 across all profiles and kinds | 8–25 min | Yes |
| FairFill | gate/fast, full workspace tests or benches | Unknown | 15–30+ min planning estimate | No |
| Longbow | dev, build CLI | Unknown, executable pool 802 outside scratch directories | 14–16 s observed | Yes |
| Longbow | test, workspace no-run | Unknown, rlib pool 6,681 outside scratch directories | 54–405 s observed | Yes |
| Longbow | dev, all-feature workspace Clippy | Unknown, metadata pool 2,548 outside scratch directories | 11–124 s observed | Yes |
| Longbow | release/profiling, CLI or bench binary | Unknown | 8–25 min planning estimate | No |
| Markets-rs | dev, Clippy with all features and defaults | Unknown, shared metadata pool 4,163 | 17–45 s each observed | Yes |
| Markets-rs | test, all-feature tests and examples | Unknown, executable pool 14,130 | 107–233 s observed | Yes |
| Markets-rs | release/research/bench, named CLI or benchmarks | Unknown | 10–30+ min planning estimate | No |
| research-rag | dev, all-feature Clippy | Unknown, metadata pool 175 | 1–10 s observed | Yes |
| research-rag | test, workspace no-run | Unknown, executable pool 397 | 5–24 s observed | Yes |
| research-rag | release, build the two deploy binaries | Unknown | 3–12 min planning estimate | Yes |

## Disk allowance

Amounts are GiB. The incremental allowances below are estimates for one
population of the added command outputs. FairFill's Core benchmark reported
42 GiB in the complete dev/test tree and 0.7–1.4 GiB in its run cache.
Markets-rs reported 22–113 GiB in the test tree and 1.1–4.0 GiB in its run
cache. Longbow reported 42–65 GiB in the test tree and about 9 GiB in its run
cache. Target usage grows with retained build variants and old versions.
Cache additions depend on cacheability, existing entries, and entry limits.
With btrfs reflinks, the dedicated checkout and cache can share physical extents.

| Project | Latest checkout output | Additional target / cache estimate | Proposed checkout cleanup threshold |
| --- | --- | --- | --- |
| FairFill | 217 GiB allocated | fast 5–20 / 1–4 GiB, release 3–12 / 0.5–3 GiB | 230 GiB allocated |
| Longbow | 111 GiB apparent, about 65 GiB on disk | 0 / 0 GiB planned | 120 GiB apparent or 80 GiB on disk |
| Markets-rs | 1,111 GiB apparent, about 762 GiB on disk | 0 / 0 GiB planned | 250 GiB apparent or 160 GiB on disk |
| research-rag | 18 GiB apparent, about 10 GiB on disk | release 2–8 / 0.5–3 GiB | 35 GiB apparent or 25 GiB on disk |

Core can use the existing locked `leto-prewarm <project> --clean` command
for a checkout already over its threshold, then compare `status.json`
after the next warm. If a checkout regularly approaches its threshold, prune
unused profile output from that dedicated checkout while holding the same
project lock.
