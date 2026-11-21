# Leto test cache shadow runner

Stage 0 records which nextest test results might be reusable. Every test runs
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
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="$PWD/target/release/leto-test-runner" \
  cargo nextest run --manifest-path /path/to/project/Cargo.toml
```

| Variable | Purpose |
|---|---|
| `LETO_TEST_CACHE=off` | Direct exec for all invocations. |
| `LETO_TEST_CACHE_DIR` | Host store. Defaults to `~/.cache/leto-test-results/v0`. |
| `LETO_TEST_CACHE_OBSERVE` | Positive sampling interval in UTC days per key. Defaults to 3. Set 1 to observe every invocation. |
| `LETO_TEST_CACHE_PROJECT` | Stable report name for a project across worktrees. Defaults to the worktree's directory name. |
| `LETO_TEST_CACHE_WORKSPACE` | Absolute worktree root when nextest omits `NEXTEST_WORKSPACE_ROOT`. The current directory is the final fallback. |
| `LETO_TEST_CACHE_ENV` | Comma-separated additional keyed names and prefix patterns ending in `*`. |
| `SCCACHE_BASEDIRS` | Colon-separated absolute roots for key normalisation in addition to the worktree. The longest matching path prefix wins. |

The key includes the binary and resolved shared-library closure, exact argument
bytes, observer profile, kernel and architecture. The environment portion
includes `CARGO_PKG_*`, `LANG`, `LC_*`, `TZ`, `RUST_*`, `PROPTEST_*` and
configured names. It uses relative manifest and workspace paths and includes
the original manifest path length. `TMPDIR`, runner configuration and the
other nextest variables are excluded from the key. The child sees all original
values.

## Observation

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

Devbox permits tracing child processes at Yama scope 1. If attachment fails,
the child runs directly and the record says `unavailable`. Linux unprivileged
seccomp requires `no_new_privs`, and programs can detect ptrace or timing
changes. Use the opt-out for privilege-changing binaries, tests that depend
on debugger attachment or trace state, and tests with timing constraints
incompatible with observation. Use the logged verdicts as evidence for review.
The observer has finite syscall coverage and the key omits inputs such as
time and ambient variables outside the configured names.

## Host store

- `digests/` contains atomic BLAKE3 memo entries keyed by device, inode,
  size, mtime and ctime. A file touched within two seconds is rehashed and
  left out of the memo. The runner checks the stamp again after hashing.
- `closures/` memoises glibc's transitive resolution by binary digest and
  loader context, including search-directory and shared-object stamps.
- `runs/YYYY-MM-DD.jsonl` contains a single append per invocation with a key,
  binary digest and mtime, identity, outcome, timings and observer result.
- `errors/YYYY-MM-DD.jsonl` records runner problems when the sidecar is
  writable. Failure before launch takes the direct exec path. Once a test
  has started, the runner uses its existing result.

## Weekly report

```sh
python3 report.py
python3 report.py --week 2026-W47 --store /path/to/measurement/store \
  --benchmark /path/to/benchmark.json
```

By default the report writes `~/data/core/test-cache/<ISO week>.{json,md}`
and updates the `latest.json` and `latest.md` symlinks. It derives prior
outcomes and compares the first binary stamp seen in each nextest run.
It counts hypothetical stage 1 passes with stable pure evidence and
hypothetical additional stage 2 passes with stable fixture-read evidence.
Eligibility also requires a matching first-attempt pass within seven days.
File candidates would require manifest verification in stage 2. Test
seconds are sums of recorded process durations and may overlap. Cumulative
setup time is reported separately from paired wall-time measurements.

Pass `--known-flakes` a JSON list of objects with `project`, `binary_id`
and `test` to label known flakes. Other outcome mismatches require
investigation. To include wall overhead, the benchmark JSON must contain
`gate_budget_reference_s` and a `runs` array with `mode` and `wall_s` per run.
Use `plain` as the baseline mode.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 -m unittest discover -s tests -p test_report.py
```

The toy-workspace nextest test reports a skip if cargo-nextest is unavailable.
