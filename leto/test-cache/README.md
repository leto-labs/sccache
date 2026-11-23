# Leto test cache runner

The default mode, `shadow`, records which nextest test results might be reusable. Every test runs
once per nextest invocation as usual, including on repeated keys and on retries.
The runner inherits the test's arguments, environment, working directory,
process group and standard streams. There is no sandbox policy, output capture,
result storage or replay. The crate is a separate Cargo workspace from sccache.

The runner supports x86-64 Linux and the system glibc loader. It uses nextest's
runtime binary ID, test name and exact-filter arguments to identify individual
test calls. Listing, ordinary Cargo commands and opt-out calls exec the binary
directly. Use nextest 0.9.130 or newer for worktree and profile metadata, or set
the workspace fallback explicitly.

```sh
cd leto/test-cache
cargo build --release --locked
runner="$PWD/target/release/leto-test-runner"
cd /path/to/project
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="$runner" cargo nextest run
```

| Variable | Purpose |
|---|---|
| `LETO_TEST_CACHE` | `shadow` (default), `off` (direct exec), `replay` (read results), or `gate` (read, verify and store results). |
| `LETO_TEST_CACHE_DIR` | Host store override for the selected mode. Use separate directories for shadow and stage 1. |
| `LETO_TEST_CACHE_OBSERVE` | Positive sampling interval in UTC days per key. Defaults to 5. Set 1 to observe every invocation. |
| `LETO_TEST_CACHE_PROJECT` | Stable report name for a project across worktrees. Defaults to the worktree's directory name. |
| `LETO_TEST_CACHE_WORKSPACE` | Absolute worktree root when nextest omits `NEXTEST_WORKSPACE_ROOT`. The current directory is the final fallback. |
| `LETO_TEST_CACHE_ENV` | Comma-separated additional keyed names and prefix patterns ending in `*`. Values enter only the key hash; records, error logs, the digest memo and reports never contain them (`keyed_env_values_never_reach_records_errors_or_reports`). Paths a test opens or probes are recorded as paths, so a secret embedded in a file path the test touches would appear. |
| `SCCACHE_BASEDIRS` | Colon-separated absolute roots for key normalisation in addition to the worktree. The longest matching path prefix wins. |

The shadow key includes the binary and resolved shared-library closure, exact argument
bytes, observer profile, kernel and architecture. The environment portion
includes `CARGO_PKG_*`, `LANG`, `LC_*`, `TZ`, `RUST_*`, `PROPTEST_*` and
configured names. It uses relative manifest and workspace paths and includes
the original manifest path length. `TMPDIR`, runner configuration and the
other nextest variables are excluded from the key. In shadow mode the child
sees all original values.

## Stage 1

`LETO_TEST_CACHE=replay` reads passes written by gate mode and runs misses
normally. It writes invocation records and leaves result files, digest memos
and quarantine unchanged. `LETO_TEST_CACHE=gate` can also store new passes
and records negative entries for impurity. Both modes bypass caching on
retries, stress runs and instrumented invocations with `LLVM_PROFILE_FILE`.

Gate uses the shadow key plus the sandbox profile digest. To store a pass it
runs the test with Landlock, seccomp and the observer. The test sees the keyed
environment and a fresh writable `TMPDIR`. File reads are restricted to the
test executable, its shared libraries and fixed system paths. The observer
checks metadata probes and denials. If a sandboxed run fails or has impure
evidence, gate reruns it with the original environment and reports that result.
A passing fallback records an impure entry. A failing fallback records only
the invocation. Gate logs and runs tests normally if Landlock ABI 6 or newer
or the observer is unavailable.

Gate verifies the first 20 hits in each nextest run (or all hits when there
are fewer), then selects each remaining hit with a deterministic 5% sample
based on the run ID and cache key. It compares the uncached exit status,
stdout and stderr, ignoring the elapsed time in libtest's final summary.
A mismatch fails the test, deletes the entry and quarantines the project,
binary ID and test name for 30 days. Use the same project name in each
worktree to share quarantine state.

Stage 1 stores results under `~/.cache/leto-test-results/v1/<prefix>/<key>`.
Entries expire after seven days. Each of the 256 shards evicts its oldest
entries at 4 MiB for a total result capacity of 1 GiB. Output over 1 MiB
is ineligible for storage. Captured output is authenticated and encrypted
with a key derived from the keyed environment so its values do not appear
in plaintext even when a test prints them.

After a gate, print the split for its nextest run:

```sh
python3 report.py --gate-summary --run-id "$run_id"
```

Use `--worktree "$PWD"` to select the latest gate recorded for that worktree.
The command exits nonzero if the selected run has a verification mismatch or
if it cannot find a matching run.

## Shadow observation

A key has one assigned observation day per sampling period. Traced runs use
ptrace with a seccomp trace filter on filesystem, socket and process calls.
The observer follows forked children and threads, resolves file descriptors
with procfs, and separates startup loader probes from application accesses.
It records a sampled verdict of `pure`, `files`, `exec`, `network` or
`other`. A verdict describes the observed run and the proposed allowlist.
Reads in the ambient environment, such as libtest looking up `TERM` in a
terminfo database, can change the verdict. Evidence in a record is bounded.
Paths prefixed with `./` are relative to the reported worktree. Classification
continues after the evidence limit is reached.

Metadata lookups on ancestors of the worktree or `TMPDIR`, including `/`,
are recorded in `ancestor_lookups`. Opening an ancestor with `O_PATH` or
`O_DIRECTORY` counts as a lookup. The observer resolves the descriptor
when a test calls `getdents` or `getdents64` and applies the usual read rules
to the directory entries. An unresolved listing makes the run impure.

Devbox permits tracing child processes at Yama scope 1. If attachment or filter
setup fails, the test runs unfiltered and the record says `unavailable`. Unprivileged
Linux seccomp requires `no_new_privs`, and programs can detect ptrace or timing
changes. Use the opt-out for privilege-changing binaries, tests that depend
on debugger attachment or trace state, and tests with timing constraints
incompatible with observation. Use the logged verdicts as evidence for review.
The observer has finite syscall coverage and the key omits inputs such as
time and ambient variables outside the configured names.

## Shadow store

Shadow uses `~/.cache/leto-test-results/v0`.

- `digests/` contains atomic BLAKE3 memo entries keyed by device, inode,
  size, mtime and ctime. A file touched within two seconds is rehashed and
  left out of the memo. The runner checks the stamp again after hashing.
- `closures/` memoises glibc's transitive resolution by binary digest and
  loader context, including search-directory and shared-object stamps.
- `runs/YYYY-MM-DD.jsonl` contains a single append per invocation with a key,
  binary digest and mtime, identity, outcome, timings and observer result.
- `errors/YYYY-MM-DD.jsonl` records runner problems when the sidecar is
  writable. Failure before launch takes the direct exec path. Once a test
  has started, the runner uses its existing result. A process-group SIGKILL can
  end the runner before it appends the record.

## Weekly report

```sh
python3 report.py
python3 report.py --week 2026-W47 --store /path/to/measurement/store \
  --benchmark /path/to/benchmark.json
```

By default the report reads both host stores, writes
`~/data/core/test-cache/<ISO week>.{json,md}`
and updates the `latest.json` and `latest.md` symlinks. It derives prior
outcomes and compares the first binary stamp seen in each nextest run.
It counts hypothetical stage 1 passes with stable pure evidence and
hypothetical additional stage 2 passes with stable fixture-read evidence.
Eligibility also requires a matching first-attempt pass within seven days.
File candidates would require manifest verification in stage 2. Test
seconds are sums of recorded process durations and may overlap. Cumulative
setup time is reported separately from paired wall-time measurements.
The Stage 1 section counts actual replays, verification results and
quarantined identities encountered during the report week.

Pass `--known-flakes` a JSON list of objects with `project`, `binary_id`
and `test` to label known flakes. Other outcome mismatches require
investigation. To include wall overhead, the benchmark JSON must contain
`gate_budget_reference_s` and a `runs` array with `mode` and `wall_s` per run.
Use `plain` as the baseline mode. An optional shared `pair` value associates
each shadow run with its plain control.

## FairFill overhead on devbox

Measured on 2026-11-21 at FairFill revision `d295a51566`. A private
`git clone --shared` used the gate build and warmed offline Forge fixtures.
The timed command was the reuse-mode nextest invocation from
`scripts/check.sh --profile fast`. Each arm used
`nice -n 10 taskset -c 0-15,32-47`, and all eight runs passed the same
8,737 tests with 159 skipped.

```sh
./scripts/cargo.sh nextest run --profile gate \
  --binaries-metadata "$scratch/binaries.json" --cargo-metadata "$scratch/cargo.json" \
  --ignore-default-filter --run-ignored default --no-fail-fast
```

| Runner | Wall runs in seconds | Median paired overhead | Share of 235 s gate |
|---|---|---:|---:|
| Plain | 28.47, 43.33, 22.61 | — | — |
| Default, N=5 | 45.17, 28.27, 27.05 | +4.44 s | 1.89% |
| Observe all, N=1 | 61.51, 51.22 | +20.46 s | 8.71% |

The default met the 7.05 s median budget. Shared CPU pressure varied across
the first two pairs, which had raw wall differences of +16.70 s and −15.06 s.
The lowest-pressure pair ran default first and added 4.44 s. The report
keeps the individual timings and CPU-pressure readings. The timed command
covers nextest, and the 235 s reference covers the warm gate.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 -m unittest discover -s tests -p test_report.py
```

The toy-workspace nextest test reports a skip if cargo-nextest is unavailable.

`report.py` reads only the day files of the report week and the week before, newest first, up to `--max-bytes` (256 MiB by default), and keeps only the fields it aggregates. When the cap cuts older records, the report says so (`input.truncated`).
