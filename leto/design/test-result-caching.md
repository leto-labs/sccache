# Design research: caching test results across worktrees and edits

Date 2026-11-21. Branch `leto` at `8e2b5b20`. Read-only research plus toy
experiments in a scratchpad (appendix). No project test suite or benchmark was
run; all project numbers come from existing evidence logs. Hosts: devbox
(FairFill), worker-01 (Longbow), worker-02 (Markets-rs); kernel 7.0.0-43,
cargo-nextest 0.9.140 on all three.

Question: test execution is now the largest part of the loop. Can a test run be
skipped when its inputs are provably unchanged, across worktrees and across
edits?

## Verdict: go-with-limits

| Part | Verdict | Note |
|---|---|---|
| Stage 0: shadow mode (compute keys, observe inputs, run everything, log would-be hits) | **go** | Cheap. Produces the two numbers this review could not measure: per-test purity and how often a rebuilt binary is byte-identical. |
| Stage 1: replay cached passes of pure tests, per test case, keyed by binary digest, run under Landlock + seccomp, observer on stored runs, passes only | **go, behind a switch, after stage 0** | Sound under the conditions in "Soundness argument". Small pool: 29% / 6% / 0.1% of test seconds at binary granularity (FairFill / Markets-rs / Longbow). |
| Stage 2: add observed, verified file manifests (fixtures, spawned workspace binaries) | **go after stage 1 soaks** | This is where the volume is: +46% / +14% / +40% of test seconds. Same shape as the verified linker manifest. |
| Stage 3: loopback-network tests in a private network namespace | **not yet** | 80% of Markets-rs test seconds. Needs unprivileged user namespaces, which AppArmor restricts on all three hosts; the workers have no `bwrap`. |
| Skipping tests of the crate being edited (`test -p X` after an edit to X) | **no-go by construction** | The binary changes, so its inputs changed. A result cache keyed on the binary cannot help here. This is the measured 88% case. |
| Caching failures | **no-go** | Go, Bazel (`auto`) and Pants do not serve them either. |

The win is in gates, new worktrees and reruns, not in the focused edit loop:

- A lane's gate reruns every test although the lane changed a few crates. By
  the package graph, 37% (FairFill), 48% (Markets-rs) and 15% (Longbow) of test
  seconds sit in binaries the lane did not touch (Q5).
- Gates repeat on identical Rust sources. The FairFill TEST4 lane changed no
  `.rs` or Cargo file and ran the full 8,682-test suite 11 times.
- FairFill evidence logs alone hold 101 full-gate nextest runs in 7 days
  (5.0 h of test wall time; 461 runs, 1.29 M test executions, 8.2 h in all).
- The build-bench edit loop (`test -p <edited crate>`) gets 0% from this
  design. Its synthetic edit would even show a misleading 100% (E2).

## Q1. What defines a test result's inputs

Evidence columns: E-numbers are the experiments in the appendix.

| Input | Evidence | Stage 1 treatment |
|---|---|---|
| Test binary | Digest covers sources, dependencies, features, profile, toolchain, `include_bytes!`, `env!` values and linked native code: everything the linked-output key and manifest pin (`leto/design/linked-outputs-review.md` Q2). | Content digest in the key. |
| Binary identity across worktrees | With `SCCACHE_BASEDIRS_REMAP_PATHS=1` restored binaries are byte-equal (linked review E4), so equal digests give equal keys with no path logic. Not for binaries that bake `env!("CARGO_MANIFEST_DIR")`: 34 of 206 FairFill and 197 of 654 Markets-rs executables (linked review Q4.7); FairFill has 67 `env!` uses and no run-time `env::var` use. | Those binaries hit only inside one worktree until the path moves to run time. |
| Dynamic libraries | E3: every test maps `libc.so.6`, `libgcc_s.so.1`, reads `/etc/ld.so.cache`. FairFill also builds a workspace dylib (`libcrc_fast-*.so`, `default-list.json`). nextest sets `LD_LIBRARY_PATH` to `target/debug/deps`, `target/debug` and the sysroot lib dir (E1). | Resolve the `DT_NEEDED` closure, digest each file, key on it. |
| Test name and arguments | E1: nextest runs `<bin> --exact <name> --nocapture`, plus `--ignored` or `--bench`; lists with `--list --format terse [--ignored]` (also [custom harness contract](https://nexte.st/docs/design/custom-test-harnesses/)). | Arguments in the key. List calls pass through. |
| Environment | E1: nextest adds `CARGO_MANIFEST_DIR`, `CARGO_PKG_*`, `CARGO_BIN_EXE_*`, `NEXTEST_*`, `LD_LIBRARY_PATH`. Per-run values: `NEXTEST_RUN_ID`, `NEXTEST_ATTEMPT_ID`, `NEXTEST_TEST_GLOBAL_SLOT`, `NEXTEST_TEST_THREADS` (64 on devbox). Project switches exist: `LONGBOW_DATA_ROOT`, `LONGBOW_S3_TEST_URL`, `MARKETS_PYTHON`, `MARKETLAB_REGISTRY` and others (grep of test code). | The test sees only keyed variables. Everything else is removed (Pants does the same). |
| Working directory | E1: the package directory. | Not readable in stage 1. Its path string is a residual risk (R8). |
| Files read at run time | Static scan: test code of binaries holding 66% (FairFill), 97% (Longbow), 84% (Markets-rs) of test seconds mentions `std::fs`; 21% / 47% / 5% mention `CARGO_MANIFEST_DIR`. FairFill fixtures escape the package directory in 7 places (`.join("../..")`, `../fairfill-indexer/tests/fixtures`). | Denied in stage 1; observed and verified in stage 2. |
| Temp files | E3: `std::env::temp_dir()` honours `TMPDIR`. Share of seconds in binaries using `tempfile`: 32% / 93% / 70%. | Private writable `TMPDIR` per test, deleted afterwards. |
| Spawned processes | `CARGO_BIN_EXE_*`: 6 FairFill, 8 Markets-rs executables (linked review); Longbow has 23 source uses, Markets-rs 42. External tools in test code: `git`, `python3`, `bash`, `docker`, `forge`, `strace`, `uptime`, `taskset`, `kill`. | `execve` is fatal in stage 1. Stage 2 adds workspace binaries by digest. |
| Network | Share of seconds in binaries whose tests mention sockets or loopback: 18% / 45% / 47%. Markets-rs runs TLS fixtures on loopback. | Socket creation denied in stage 1. |
| Clock | E3: `SystemTime::now()` makes no system call (vDSO). No tracer or filter sees it. 62% / 89% / 74% of seconds are in binaries that mention a clock or sleep. | Cannot be captured. Bounded by an entry age limit and an uncached nightly run (R1). |
| Randomness | E3: every test calls `getrandom` (std `HashMap` seeds), so the call says nothing. | Not an impurity signal. Seeded property tests are deterministic; unseeded ones are flaky and handled as flakes. |
| CPU count, cgroup | E3: every libtest process calls `sched_getaffinity` and reads `/proc/self/cgroup` and `/sys/fs/cgroup/**/cpu.max`. | Allowed, not keyed (R7). |
| Order and shared state | nextest gives one process per test ([why](https://nexte.st/docs/design/why-process-per-test/)); what remains is the file system and ports. | Removed by the private `TMPDIR` and the read deny. |

### How other systems capture these

| System | Key | Run-time inputs | Failures | Enforcement |
|---|---|---|---|---|
| Bazel | Test action and its declared dependencies. "Bazel will only rerun a test if" it "detects changes in the test or its dependencies", the test is `external`, `--runs_per_test` is used, or "the test failed" ([user manual](https://bazel.build/docs/user-manual)). | Declared `data` (runfiles), `env`, `env_inherit`, `--test_env`. | Saved, not served under `auto`; `yes` "may cache test failures". | `linux-sandbox` uses "User, Mount, PID, Network and IPC namespaces" and "makes the entire filesystem read-only except for the sandbox directory"; `processwrapper-sandbox` only builds a symlink forest and "doesn't hide the host environment" ([sandboxing](https://bazel.build/docs/sandboxing)). Tags: `external` ("force test to be unconditionally executed"), `no-cache`, `local`, `block-network`, `requires-network` ([common definitions](https://bazel.build/reference/be/common-definitions)). |
| Go `go test` | Test binary and cacheable flags, then a second key from the replayed log of the last run. | **Observed**: the `testing` runtime logs `getenv`, `open`, `stat`, `chdir`. Files are hashed by size, mode and mtime, not content; a file modified in the last 2 s forces a miss; paths outside the module, GOPATH or GOROOT are skipped ([`test.go`](https://github.com/golang/go/blob/master/src/cmd/go/internal/test/test.go)). | "Test caching applies only to successful test results" ([Go 1.10](https://go.dev/doc/go1.10)). | None. Subprocesses, network and time are not tracked. |
| Pants | Declared sources and dependencies per test file. | Declared `files`/`resources`; "Test runs are hermetic, meaning that they are stripped of the parent `pants` process's environment variables" ([test goal](https://www.pantsbuild.org/stable/docs/python/goals/test)). | "caching of successful tests means that rerunning ... will ... only rerun the failures". | Temp-dir sandbox; "Pants' caching relies on all process being reproducible based solely on inputs in the repository". |
| Turborepo | Global hash and task hash: `inputs` globs (default: tracked files of the package), `env`, lockfile ([caching](https://turborepo.dev/docs/crafting-your-repository/caching)). | Declared only. `envMode: "strict"` filters the environment. | Outputs are cached "when the task is successfully completed". | None. |
| Gradle | Declared task inputs; `Test` is a cacheable task ([build cache](https://docs.gradle.org/current/userguide/build_cache.html)). | Declared; "Missing task inputs can cause incorrect cache hits". | Not stated in the pages read. | None. |
| BuildXL | Weak fingerprint (declared inputs), then path sets of "observed file accesses (reads, probes, directory enumerations)", then a strong fingerprint with the hashes of those files ([two-phase lookup](https://github.com/microsoft/BuildXL/blob/main/Documentation/Wiki/Advanced-Features/Two-Phase-Cache-Lookup.md)). | Observed by a sandbox, including absent-file probes. | Not checked. | Sandbox observes and blocks. |
| Nix | The derivation hash; `checkPhase` runs inside the build ([nixpkgs phases](https://nixos.org/manual/nixpkgs/stable/#ssec-check-phase)). A cached result follows from the output path (inference). | All inputs are derivation inputs. | A failed check fails the build (inference). | Build sandbox with private namespaces. |
| Buck2 | No test-result caching is described in the pages read ([test command](https://buck2.build/docs/users/commands/test/), [test execution](https://buck2.build/docs/rule_authors/test_execution/)). | | | |
| nextest `--rerun` | Experimental record store. "Only tests that didn't pass in the specified run will be executed" (`run --help`). | None: "works purely at the test level, and does not track code or build changes" ([rerun](https://nexte.st/docs/features/record-replay-rerun/rerun/)). | | Safe for a rerun on the same tree, not as a cache. |
| cargo | None. Issue [rust-lang/cargo#10673](https://github.com/rust-lang/cargo/issues/10673) "Don't re-run tests that already passed" is open; the objection there is that Cargo does not know the external inputs. | | | |

Guarantees the sandboxed systems rely on that Leto does not have today:

1. A complete declared input set. Bazel has `data` and runfiles; Cargo has no
   declaration for run-time data.
2. A stable execution root. Bazel and Nix run at a fixed relative or absolute
   path. Leto worktrees differ in their absolute path.
3. A mount namespace, so an undeclared path does not exist. On these hosts
   `kernel.apparmor_restrict_unprivileged_userns = 1`; `bwrap` works on devbox
   only and is not installed on the workers.
4. A constant environment (Bazel: `TZ=UTC`, fixed `PATH`, locale unset,
   [test encyclopedia](https://bazel.build/reference/test-encyclopedia)).

None of them captures the clock. Bazel states "The current time and date are
unspecified" and also "such behavior is not enforced".

## Q2. Input capture options

Measured on devbox, niced, toy binaries (E4, E5). "Path call" means a system
call that takes a path.

| Mechanism | Sees or blocks | Misses | Cost | Available here |
|---|---|---|---|---|
| (a) Declared inputs only | What the manifest lists | Everything not listed; nothing checks the list | None | Yes. Unsound alone: a missed input is a stale pass. |
| LD_PRELOAD hook | libc `open`, `stat`, `execve` | Static binaries and raw system calls. rustix "uses raw Linux system calls" and "is enabled by default on platforms which support it" ([README](https://github.com/bytecodealliance/rustix/blob/main/README.md)); `tempfile` and others use rustix. | Low | Not sound for Rust. Rejected. |
| fanotify | File events | Unprivileged groups cannot "request permission events", are "limited to only mark inodes", and do "not receive the pid that generated the event" ([fanotify_init(2)](https://man7.org/linux/man-pages/man2/fanotify_init.2.html)) | | Needs `CAP_SYS_ADMIN`. Rejected. |
| ptrace (`strace -f --seccomp-bpf`, path, network and process calls only) | Every such call with its result, in all children; sees `stat` and denied calls | vDSO clock, io_uring operations, memory-mapped reads after open (the open is seen) | +8 ms per process; +37 µs per path call (0.40 s → 1.00 s for 15,051 path calls); CPU-bound test: none. Without `--seccomp-bpf` the same test takes 3.1-3.7 s. | Yes (Yama allows tracing children). A traced test cannot itself be traced; Markets-rs has tests that spawn `strace`. |
| seccomp user notification | Selected calls, supervisor reads paths | Documented race on pointer arguments; "must not be used to make security policy decisions" ([seccomp_unotify(2)](https://man7.org/linux/man-pages/man2/seccomp_unotify.2.html)) | Not measured; fewer stops than ptrace | Yes. An option for a faster observer later; detection only. |
| Landlock + seccomp (restrict) | Blocks file reads, writes and execution outside listed paths; TCP bind and connect; abstract UNIX sockets and signals out of the domain. seccomp blocks `socket()` and can kill on `execve`. | Does not restrict "chdir(2), stat(2), flock(2), chmod(2), chown(2), setxattr(2), utime(2), fcntl(2), access(2)" ([Landlock](https://docs.kernel.org/userspace-api/landlock.html)). UDP needs ABI 10 and pathname UNIX sockets ABI 9; the hosts have ABI 8. Denials return `EACCES`, they do not kill, and the audit log is not readable without privilege. | +1.0 ms per process (2.45 → 3.43 ms); file-heavy test: none measurable | Yes, unprivileged, on all three hosts (`landlock` in `/sys/kernel/security/lsm`). |
| bwrap (mount, PID and network namespaces) | Only bound paths exist; private loopback | Clock; shared kernel state | +10 ms per process (12.5 ms) | devbox only. |

Soundness of each class:

- **(a) declared only**: a false negative needs only a forgotten line. No.
- **(b) observed only** (Go's model): complete for what the observer sees.
  ptrace sees all file, network and process calls. It gives no protection
  when the observer has a gap (io_uring), and a pure observation is of one
  run: a test that reads a file only on some days is recorded without it.
- **(c) declared and enforced** (Bazel's model): the undeclared world is
  uniformly invisible, so the result cannot depend on it. Landlock alone
  leaves two holes: metadata calls (`stat`) are not blocked (E4: `stat_only`
  passes and would be cached), and a test that swallows `EACCES` and skips its
  body passes vacuously. A swallowed denial is the realistic one: "tool not
  found, skipping" after a denied `execve`, or `File::open(..).is_err()`.

Recommended for the first stage: **enforce and observe together on every run
that stores a result**. Landlock and seccomp bound what the test can reach;
the observer proves that nothing was denied and nothing outside the allowed
set was even probed. Each layer covers the other's gap: Landlock blocks
io_uring file opens that the tracer cannot see, and the tracer sees the `stat`
calls and the denied calls that Landlock cannot report. A run that only looks
up (no store) needs neither.

False positives (needless reruns) cost time only: a denied access, a probe
outside the allowed set, a recently modified file, or an unknown variable all
mean "run it and do not store".

## Q3. Granularity and integration point

**Per test case, not per binary.**

- nextest already runs one process per test with a stable command line (E1),
  so a per-test key needs no change to nextest or libtest.
- Purity is mixed inside binaries. At binary granularity a binary is as
  impure as its worst test: only 29% / 6% / 0.1% of seconds are in binaries
  with no file, process or network signal (Q5).
- A gate rerun after one failure then runs one test, not one binary. Longbow's
  lane reran the whole workspace after isolated failures (2,142 s and 2,893 s
  runs, `REPORT.md`, `failures.md`).
- Cost: one runner start and one small file read per test. All tests of a
  binary still invalidate together, because the digest is in every key.

`cargo test` (one process, many tests) and doctests are out of scope: the
runner passes them through.

Integration points compared:

| Option | Facts | Assessment |
|---|---|---|
| nextest target runner | Stable. "Nextest invokes target runners during both the list and run phases"; runner output "MUST NOT" go to stdout while listing ([target runners](https://nexte.st/docs/features/target-runners/)). Set by `CARGO_TARGET_<TRIPLE>_RUNNER` or `.cargo/config.toml`. Global for the invocation. E1 and E6 confirm it. | **Recommended.** All three projects call `scripts/cargo.sh nextest run`, so one exported variable enables it per invocation with no repository change. |
| nextest wrapper script | Experimental (`experimental = ["wrapper-scripts"]`, since 0.9.98). `[scripts.wrapper.<name>]`, bound by `[[profile.<p>.scripts]]` with `filter`, `platform`, `list-wrapper`, `run-wrapper` ([wrapper scripts](https://nexte.st/docs/configuration/wrapper-scripts/)). | Good later for scoping by filterset. Needs an experimental flag in each repository. The runner can scope itself from `NEXTEST_BINARY_ID` and `NEXTEST_TEST_NAME`. |
| nextest setup script | Experimental; can export variables through `$NEXTEST_ENV` ([setup scripts](https://nexte.st/docs/configuration/setup-scripts/)). | Not needed; the driver does the same job. |
| cargo subcommand (driver) | Not a hook by itself. | **Recommended as the front end**: sets the runner, hashes the binaries once per run, applies the policy, prints "N of M replayed". |
| sccache as the runner | Would add a client mode and protocol to the fork. | No. A 20 MB binary started per test, a server dependency for an 8,000-call hot path, and one more divergence from upstream (`leto/design/upstreaming-plan.md`). Reuse its code, not its process. |
| nextest archive, `--binaries-metadata` | FairFill and Markets-rs gates already run from `--binaries-metadata` and `--cargo-metadata` (`scripts/check.sh`). | Unchanged; the driver reads the same file to find the binaries. |
| JUnit, libtest-json | JUnit has `time`, no cached marker; `libtest-json` is experimental with an unspecified format ([JUnit](https://nexte.st/docs/machine-readable/junit/), [libtest-json](https://nexte.st/docs/machine-readable/libtest-json/)). | A replayed test shows as `PASS` in about 0.02 s. The runner keeps its own event log; the driver reports from it. |

## Q4. Failure modes and the opt-in model

| Hazard | Rule |
|---|---|
| Failing test | Never stored. A run that fails under the sandbox is repeated without it and its result is reported; nothing is stored. |
| Flaky test | A pass is stored only when `NEXTEST_ATTEMPT=1`, retries and stress mode are off. If a key that holds a pass later fails in a verification run, the test identity is quarantined (never cached) for 30 days. |
| Time-dependent test | Not detectable (E3). Entries expire after 7 days; a nightly uncached run on main is the ground truth. A static scan for `SystemTime::now`, `Utc::now`, `Local::now` feeds an advisory deny-list. |
| Network, external services (RPC, S3, docker, forge) | `socket()` is denied and `execve` is fatal, so such tests fail under the sandbox and run unsandboxed, uncached. |
| Shared temp state | Private `TMPDIR`; every other path is read-denied. |
| `#[ignore]` | `--ignored` is part of the key. No special case. |
| Profile, features, toolchain | In the binary digest. |
| Harness environment | Only keyed variables reach the test. Per-run nextest variables are removed. |
| A file whose content differs in another worktree | Stage 1: unreadable. Stage 2: in the manifest by content, checked on every hit. |
| Coverage, sanitizers, `INSTA_UPDATE`, result files such as Longbow's `SF1R_OUT` | Writes outside `TMPDIR` are denied; the variables are not keyed by default, so the test never sees them in a cacheable run. Instrumented builds are refused when `LLVM_PROFILE_FILE` is set. |
| Custom harnesses (`harness = false`) | Same command contract; opt-in only. |

Model: three lists in one host or project file, default off.

1. **Eligible** (opt-in): nextest filtersets or package names. Start with
   packages whose shadow data shows only pure tests.
2. **Never** (opt-out): test identities that must always run. Seeded from the
   static signals (process, network, docker, clock), from the quarantine and
   from projects' own knowledge. A test in both lists is never cached.
3. **Environment**: variable names and prefixes that tests may see (for
   example `LONGBOW_*`, `MARKETS_*`, `MARKETLAB_*`, `FAIRFILL_*`, `RUST_*`,
   `PROPTEST_*`, `TZ`, `LANG`). Their values enter the key.

A test never needs a marker to be safe: an unlisted or impure test simply
runs. A marker is only needed in stage 2 to widen what a test may read beyond
its package directory.

## Q5. Wins from real data

Sources:

| Project | Run | Tests | Binaries | nextest wall | Sum of test times |
|---|---|---|---|---|---|
| FairFill | `~/data/fairfill/test4/gate-ab/warm-after.log` | 8,682 | 206 (268 listed) | 100.6 s | 1,582 s |
| FairFill, loaded host | `test4/handoff-check.log` | 8,707 | 206 | 500.7 s | 4,565 s |
| Longbow | `TEST-TURNAROUND-20261121/workspace-historical-rank.tsv` (gate `95c563d22`) | 7,921 timed of 8,920 | 57 | test stage 852-1,031 s | 4,435 s |
| Markets-rs | `~/data/markets-rs/test-turnaround/r2/gate-tmpfs.junit.xml` | 10,768 | 621 | 240.9 s | 2,189 s |

Composition: unit-test binaries hold 87% (FairFill), 35% (Longbow), 53%
(Markets-rs) of test seconds. Tests of 10 s or more are 24 / 101 / 49 tests
and 41% / 57% / 47% of the seconds. `longbow-bench` alone is 51% of Longbow.

### Test time in binaries that did not change

Method: workspace graph from `cargo metadata --no-deps`; a test binary of
package P is affected by a change to X when X is in P's dependency closure
(dev-dependencies at the first level; a change to the root `Cargo.toml`,
`Cargo.lock` or toolchain file affects all). Changes come from `git log`:
500 first-parent commits on main (one per merged lane) and up to 2,000
non-merge commits. Wall time is modelled by list scheduling on 16 slots.

| | FairFill | Longbow | Markets-rs |
|---|---|---|---|
| Workspace packages | 76 | 17 | 104 |
| Per merged lane: unaffected share of test seconds, mean (median) | 37% (23%) | 15% (13%) | 48% (58%) |
| Per merged lane: modelled wall saving if all unaffected tests are replayed | 31% | 15% | 46% |
| Per non-merge commit: unaffected share, mean (median) | 53% (56%) | 16% (13%) | 50% (53%) |
| One-crate edit, weighted by how often each crate is edited: affected share | 33% | 79% | 42% |

This is an upper bound on what the graph can prove unchanged, and a lower
bound on what digests can: E2 shows a dependent's binary stays byte-equal
after a comment edit, a dead-code edit, or a `#[cfg(test)]` edit in a
dependency. How often that happens on real edits is not known (open
question 2).

FairFill and Longbow already select affected packages in the edit loop
(`scripts/test-affected.sh`, `just affected`). A result cache is the exact,
automatic form of that selector and extends it to gates, which run
everything.

### Pure tests versus tests with fixtures or processes

Static scan of test code (files under `tests/`, test files and `#[cfg(test)]`
tails), per binary, weighted by the times above. It sees only what the test
code names, so it under-counts impurity reached through library code and
over-counts at binary granularity.

| Class (binary judged by its worst test) | FairFill | Longbow | Markets-rs |
|---|---|---|---|
| No file, process or network signal | 113 binaries, 1,994 tests, 28.7% of seconds | 13, 271, 0.1% | 310, 2,695, 5.9% |
| Files only (fixtures, temp dirs) | 55, 4,057, 45.9% | 11, 933, 40.0% | 173, 1,976, 14.1% |
| Process or network | 38, 2,631, 25.5% | 33, 6,717, 59.8% | 138, 6,097, 80.0% |

The per-test split inside mixed binaries is the main unknown; stage 0
measures it.

### Expected saving

| Scenario | Saving | Basis |
|---|---|---|
| Edit round, `test -p <edited crate>` (build-bench: Longbow 4.9 s build + 34.5 s test; Markets-rs about 11 s + 25-35 s) | **0%** | The binary under test changes each round. Use lookup-only mode so the loop pays nothing. |
| Edit round with a doc-only or dead-code edit | up to 100% | E2. The build-bench marker edit (`pub const BUILD_BENCH_ROUND`) is this case; do not use it to measure this feature. |
| Edit round over affected packages (FairFill TEST4: 2,895 tests, 78 binaries, 74.5-75.8 s) | small, unknown | Only binaries that link none of the changed code hit. |
| Lane gate after merging main | stage 1: 0.29 × 0.37 ≈ 11% of FairFill test seconds, 0.06 × 0.48 ≈ 3% Markets-rs, about 0% Longbow. Stage 2: up to 0.75 × 0.37 ≈ 28%, 0.20 × 0.48 ≈ 10%, 0.40 × 0.15 ≈ 6%. | Class share × unaffected share, binary granularity (lower bound for the class share). |
| Gate repeated on unchanged Rust sources (TEST4: 11 full runs, no `.rs` or Cargo change in six commits) | stage 1: at least 29% / 6% / 0.1% of each repeat; stage 2: at least 75% / 20% / 40% | Class share × 100%. |
| New worktree, first gate at a commit already tested elsewhere | Same as the row above. FairFill: 100.6 s wall → about 71 s (stage 1) or about 25 s (stage 2), by seconds share; 1,582 CPU-s → 1,128 or 400. | A fully replayed test costs about 3 ms (E6), so 8,682 tests cost about 26 CPU-s. |
| Fleet, FairFill on devbox | 101 full-gate runs in 7 days: 5.0 h wall, 44-128 CPU-h at 1,582-4,565 CPU-s per run. Each percent replayed returns that share to a host that runs at load 40 or more. | `~/data/fairfill/**/**.log` since 2026-11-14; evidence logs only, so a lower bound on runs. |

Loaded hosts inflate test seconds 2.4-2.9 times (Markets-rs 5,194 s against
2,189 s; FairFill 4,565 s against 1,582 s), so removed test CPU also shortens
everyone else's runs. This second-order effect is not estimated.

## Q6. Correctness gates

Cached passes are the same class of trust the projects already place in
cached compiler outputs: the gate links test binaries that sccache restored.
Policy by run type:

| Run | Mode | Rule |
|---|---|---|
| Focused edit loop | `read` | Replay hits, run misses plain, store nothing. No sandbox or observer cost. |
| Lane gate, handoff gate (`scripts/check.sh`) | `read-write` | Replay only entries stored by a gate-profile run, at most 7 days old. Rerun a random 5% of the hits (at least 20 tests) uncached and compare; a mismatch fails the gate, deletes the key and quarantines the test. Print the replayed count in the gate log. |
| Nightly on main, release qualification | `off` | Everything runs, unsandboxed. Its results are compared with the store; a stored pass that fails there is a finding. |

The lane gate keeps its meaning because every test in a binary the lane
changed has a new digest and must run. What is replayed is a test whose
binary and inputs equal a run that passed. Bazel users accept the same thing
in CI; `--cache_test_results=no` is their periodic run.

The gate log must state the split, for example
`8,682 passed (5,930 replayed, 297 verified by rerun)`, because the projects
keep these logs as evidence.

## Recommended architecture

### Components

1. `leto-test-runner`: a small binary used as the nextest target runner.
   Invoked as `leto-test-runner <test-binary> <args>`.
2. `cargo leto-test` (or a mode of each `scripts/cargo.sh`): the driver. Sets
   `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER`, writes a per-run index of
   binary digests, runs nextest, reads the event log, prints the summary and
   applies the gate rules.
3. A sidecar store, one per host.

Both binaries live in this repository beside the fork (`leto/test-cache/`),
outside the sccache crate, and reuse its code: `Digest`, the stat-keyed
`Stamp` and `file_digest` memo rules and `LinkPath` roots from
`src/compiler/rust/linked.rs` (lines 357-436, 733-760), and the basedir
normalisation in `src/config.rs` (1535-1575).

### Key (stage 1)

`K = blake3(` in this order `)`:

1. Format version and the digest of the sandbox profile (allowed paths,
   seccomp program, environment rules).
2. Content digest of the test binary. Memoised by size, mtime, ctime, device
   and inode; a file changed in the last 2 s is hashed again and not
   memoised (the linked-manifest rule). The driver hashes all binaries of
   the run once: at 80 ms per 100 MB, 268 binaries of 100 MB (the Markets-rs
   mean is 106 MB; FairFill sizes were not measured) cost about 21 CPU-s
   cold, near zero when memoised. The fork can later write the
   digest as an xattr when it stores or restores an executable.
3. The resolved shared-library closure: path and content digest of each
   object, including the dynamic loader.
4. The arguments after the binary, verbatim.
5. The environment the test will see, sorted: `CARGO_PKG_*`, the configured
   names and prefixes, and `CARGO_MANIFEST_DIR` and `NEXTEST_WORKSPACE_ROOT`
   with the basedir replaced as sccache does for `cwd`. The length of the
   real `CARGO_MANIFEST_DIR` is added (R8).
6. Host class: kernel release and CPU architecture. The store is per host
   in stage 1, so this only guards kernel upgrades.

Not in the key: the worktree path, `TMPDIR`, nextest's per-run variables (all
removed from the test's environment), time, CPU count.

### Run flow

```
list call (--list)            -> exec the binary unchanged
mode off, or not eligible     -> exec the binary unchanged
lookup K
  pass entry, not expired     -> replay stdout and stderr, exit 0, log "hit"
  "impure" entry              -> exec the binary unchanged, log "impure"
miss, mode read               -> exec the binary unchanged
miss, mode read-write         -> run under sandbox + observer
  exit 0, attempt 1, observer clean -> store pass (output, duration, host, time)
  exit 0, observer not clean        -> store "impure" for K, report pass
  non-zero or killed                -> run again unsandboxed, report that result;
                                       if it passes, store "impure" for K
```

The "impure" entry is a negative cache: it only prevents a second sandbox
attempt for the same binary and test. A real failure costs one extra run.

### Sandbox profile (stage 1)

- Landlock: read and execute on the test binary, the shared-library closure,
  `/etc/ld.so.cache`, `/proc`, `/sys/fs/cgroup`, `/dev/null`, `/dev/urandom`;
  read and write on a fresh `TMPDIR`. TCP bind and connect handled with no
  rule. Abstract UNIX socket and signal scopes. `PR_SET_NO_NEW_PRIVS`.
- seccomp: `socket()` returns `EACCES` for every family (socketpair stays);
  `execve` and `io_uring_setup` kill the process (the launcher starts the
  test with `execveat`).
- Observer (ptrace with a seccomp prefilter on path, network and process
  calls): the run is clean when no traced call was denied and every path
  argument, including `stat` and `access`, resolves inside the allowed set
  or to a system path that does not exist. Path resolution and metadata
  lookups on ancestors of the checkout or `TMPDIR` (including `/`) are
  allowed and recorded separately. The usual read rules apply to directory
  listings.
- Environment: exactly the keyed variables plus `TMPDIR`.

E4 shows the behaviour without the observer: pure tests, temp-file, clock
and CPU-count tests pass; the fixture read, the loopback socket and the
spawned binary fail with `PermissionDenied`.

### Store

A directory per host, `~/.cache/leto-test-results/v1/<2 hex>/<K>`, one small
file per entry (status, captured output up to 1 MiB, duration, stored-at,
host, binary identity, profile that stored it), written by rename. No
daemon. Expiry by mtime (7 days), size cap by oldest first. Entries with
larger output are not stored.

Why not the sccache disk cache: entries are a few hundred bytes and are read
8,000 times per gate; the sccache LRU is sized for 1-200 MB entries; a
request per test needs the server (port 4226) and its rollout process; and
test results should be deletable without touching compile outputs.
Cross-host sharing is out of scope; if it comes, it belongs in the cross-host
tier with signed entries.

### Stages

| Stage | Adds | Exit criterion |
|---|---|---|
| 0 Shadow | Runner computes K, runs every test unsandboxed under the observer, logs the path set, the denials a stage-1 sandbox would cause, and whether K was seen before with which outcome. Nothing is skipped. | One week on three projects: no would-be hit with a different outcome that is not a known flake; per-test purity and digest-stability numbers recorded. |
| 1 Pure | The flow above, eligible list from shadow data, `read` in edit loops, `read-write` in lane gates with the 5% rerun. | Two weeks without a verification mismatch; nightly uncached run agrees. |
| 2 Files | Landlock additionally allows reading the checkout. The observer's path set (reads, `stat`, directory listings, absent probes) is stored as a manifest under K; a hit requires every entry to verify by content, as the linker manifest does. Spawned workspace binaries are allowed by digest and traced. Declared tools (`git`, `python3`, `forge`) by digest. | Same as stage 1, plus the manifest tests below. |
| 3 Loopback | Private network namespace. | Needs a host decision on user namespaces. |

## Soundness argument (stage 1)

A stored pass for K is replayed only if all of these held for the run that
stored it:

1. The process could read only the binary, its libraries, fixed system
   paths and a private empty directory (Landlock), could create no socket
   and start no program (seccomp).
2. No access was denied and the observer saw only the path probes allowed
   by the sandbox profile, so the test did not take a branch that depends
   on the sandbox.
3. It saw only environment variables whose values are in K.
4. It was the first attempt and exited 0.

Then the result is a function of K, the clock, the kernel's scheduling and
resource state, and the path strings of the worktree. The first is fixed on a
hit. The others are the residual risks R1, R6, R7 and R8.

## Risks and mitigations

| # | Risk | Mitigation |
|---|---|---|
| R1 | Clock or date dependence (certificate expiry, date arithmetic). Invisible to every mechanism here. | 7-day expiry; nightly uncached run; advisory deny-list from the static scan. |
| R2 | A test passes vacuously because the sandbox denied something. | Observer on every stored run; a denial means "impure". `execve` and io_uring are fatal, not silent. |
| R3 | Landlock does not block `stat`, `access`, `chdir`. | The observer checks these calls. Without the observer (a "lite" mode) this hole is open; do not ship a lite mode. |
| R4 | Observer gap (io_uring, new system calls). | Landlock still blocks the read; `io_uring_setup` is fatal; unknown path-taking calls in the seccomp program are treated as traced. |
| R5 | Stale digest from the stat-keyed memo. | The linked-manifest rules: stamp includes ctime and inode, no memo for files changed in the last 2 s, re-stat after hashing. |
| R6 | Flaky tests: one lucky pass is replayed and hides the flake. | First attempt only; quarantine on any mismatch; the nightly run keeps a flake census. Longbow and Markets-rs have known load-sensitive tests (`failures.md`, `flaky-tests-20261113`). |
| R7 | Host state: CPU mask, memory, load, kernel. Every libtest process reads the CPU mask (E3). | Per-host store; kernel release in K; not keyed otherwise. A test whose verdict depends on the CPU count is a flake under R6. |
| R8 | Path-string dependence across worktrees (path length limits such as the UNIX socket path limit Markets-rs recorded in `r2/unix-socket-path-limit.txt`; paths in messages). | T3 worktree paths have equal length and the length is in K; cross-worktree hits are part of the 5% rerun. A mount namespace with a fixed path removes it (stage 3). |
| R9 | The runner writes to stdout during listing, or does not forward signals; nextest then mis-lists, or reports `LEAK` or a hang on timeout. | List calls `exec` the binary directly. Tests for timeout kill, `SIGINT`, and process-group cleanup. |
| R10 | Evidence logs say "8,682 passed" without saying how. | The driver prints and stores the split; the gate fails if the event log is missing. |
| R11 | Replay hides wanted side effects (coverage data, snapshot updates, result files). | Such runs set variables that are not keyed, or use instrumented binaries, and are refused; `off` mode is one flag. |
| R12 | A kernel, libc or runner upgrade changes behaviour. | Library closure, kernel release and profile digest are in K. |
| R13 | Store tampering by a process of the same user. | Same trust as the sccache disk cache today. No cross-user or cross-host sharing in these stages. |
| R14 | Observer overhead on file-heavy tests (2.2 times in E5). | Paid only on stored runs. Edit loops use `read`. The driver skips storing for a test whose last three keys were never hit. |
| R15 | Benchmarks of this feature with the build-bench marker edit report a false 100%. | Measure with a body edit of a called function. |

## Test plan for an implementation

Unit tests (runner crate):

1. K changes with: binary bytes, each argument, each keyed variable, a
   library digest, the profile digest, the kernel release. K does not change
   with: checkout directory (equal length), `TMPDIR`, `NEXTEST_RUN_ID`,
   `NEXTEST_ATTEMPT_ID`, slot variables.
2. Environment filter: only listed names and prefixes pass; path values are
   basedir-normalised in K and real in the process.
3. Digest memo: a file rewritten with the same size inside 2 s is hashed
   again; a changed inode or ctime misses.
4. Store: concurrent writers of one key leave one complete entry; a truncated
   entry is a miss; expiry and size cap; oversized output is not stored.
5. Observer verdicts from recorded traces: clean run; denied open; `stat`
   outside the set; absent system path; `socket`; `execve`; a child process.

End to end, on a toy workspace like the one in the appendix, under
`leto/test-sandboxed.sh`:

6. Listing is byte-identical with and without the runner, including
   `--ignored`.
7. Second run replays every pure test; nextest reports them passed; the
   driver prints the count.
8. One test body edited: only that binary's tests run (E6).
9. A fixture changed with the binary unchanged: the fixture test runs and
   fails (E6). In stage 2: it misses through the manifest.
10. Each impurity alone: fixture read, loopback socket, spawned binary,
    `stat` of an outside path, write outside `TMPDIR`, io_uring. None is
    stored as a pass; each still reports its true result.
11. A test that swallows a denied open and returns: stored as "impure", never
    as a pass.
12. A keyed variable changed: miss. An unkeyed variable set in the shell:
    the test does not see it.
13. Failing test: never stored; the reported output is from the unsandboxed
    run. A test that fails once then passes on retry: not stored.
14. `read` mode stores nothing and starts no sandbox. `off` mode runs the
    binary directly.
15. Timeout: nextest's `slow-timeout` terminates a sandboxed and traced test
    and its children; no `LEAK`.
16. Two worktrees with remapped paths: a pass stored in A replays in B; a
    binary that bakes `CARGO_MANIFEST_DIR` does not.
17. Gate rule: with a poisoned entry (a stored pass for a test that now
    fails) and a 100% rerun sample, the gate fails and the key is deleted.
18. Expiry: an 8-day-old entry is not replayed.

Soak (stage 0, then stage 1):

19. Shadow on FairFill, Longbow and Markets-rs for a week. Report: would-be
    hits whose real outcome differed; share of tests and seconds that are
    pure; share of rebuilt binaries with an unchanged digest; observer
    overhead per project.
20. Stage 1 with a 100% rerun sample for three days before dropping to 5%.

## Open questions

1. Per-test purity. The static scan gives 29% / 6% / 0.1% at binary
   granularity; the real per-test share needs stage 0.
2. Digest stability. How often does a dependent test binary stay byte-equal
   after a real upstream edit (E2 shows it for comments, dead code and
   `#[cfg(test)]` code)? Debuginfo level and codegen-unit layout matter.
   Stage 0 logs it.
3. User namespaces on the hosts. An AppArmor profile for the runner would
   give stage 3 (private loopback, fixed checkout path) and close R3 and R8
   without the observer. Markets-rs depends on this for most of its test
   time.
4. Longbow. Half of its test time is `longbow-bench` with generated TPC data,
   child processes and `LONGBOW_*` switches. Is it worth more than `read`
   mode and the gate-rerun benefit there?
5. A faster observer (seccomp user notification instead of ptrace) if R14
   shows in the soak.
6. Where the code lives: this repository (reuses fork code, same install
   path) or Core. Upstream nextest may accept a "rerun only tests whose
   binary changed" mode on top of its record store; worth asking once stage
   0 data exists.
7. The focused edit loop (88% test time) is untouched. Candidates outside
   this design: splitting the large unit-test binaries, running the edited
   module's tests first, coverage-based selection (cargo-difftests, nightly
   only).
8. `CARGO_MANIFEST_DIR` at run time instead of `env!` in FairFill (67 uses),
   so those binaries become shareable across worktrees. Already a
   recommendation of the linked-outputs review.

## Appendix: experiments and data

Toy workspace in the session scratchpad: `dep` (lib), `app` (lib with unit
tests `pure_add`, `pure_hashmap`, `reads_fixture`, `uses_tmp`,
`net_loopback`, `reads_clock`, `cpu_count`; a bin; integration tests
`runs_bin` with `env!("CARGO_BIN_EXE_app")` and `stat_only`), `io`
(`file_heavy`: 3,000 files written, read, stat-ed and removed; `cpu_heavy`).
rustc 1.97.1 on devbox, `RUSTC_WRAPPER` empty, commands niced on CPUs
0-15,32-47. Timings are single runs or means of 300 on a shared host.

- **E1** nextest contract. A logging target runner sees 8 list calls
  (`--list --format terse`, and with `--ignored`, per binary) and one
  `--exact <name> --nocapture` call per test, cwd = package directory. The
  environment nextest adds is listed in Q1.
- **E2** Digest stability. Baseline digests of four test binaries, then one
  edit each, at `debug` 0, `line-tables-only` and 2, incremental on and off
  (same result in all six):

  | Edit | `dep` tests | `app` unit tests | `app` bin tests | `app::it` |
  |---|---|---|---|---|
  | `dep`: doc comment text, same line count | same | same | same | same |
  | `dep`: comment line inserted at the top | changed | changed | same | same |
  | `dep`: body of a function `app` does not call | same | same | same | same |
  | `dep`: `#[cfg(test)]` body | changed | same | same | same |
  | `dep`: build-bench marker (`pub const` appended) | same | same | same | same |
  | `dep`: body of a function `app` calls | changed | changed | same | same |
  | `app`: one unit test body | same | changed | same | same |

- **E3** `strace -f` of single tests. Every test: `/etc/ld.so.cache`,
  `libc.so.6`, `libgcc_s.so.1`, `/proc/self/maps`, `/proc/self/cgroup`, the
  `cpu.max` file of each cgroup level, `sched_getaffinity`, `getrandom`.
  `reads_clock` adds no
  call. `uses_tmp` writes under `$TMPDIR`. `net_loopback` calls `socket`.
- **E4** Landlock (ABI 8) and seccomp launcher, allowed: binary, `/usr`,
  `/lib`, `/lib64`, `/etc/ld.so.cache`, `/proc`, `/sys/fs/cgroup`,
  `/dev/null`, `/dev/urandom`, private `TMPDIR`. Pass: `pure_add`,
  `pure_hashmap`, `uses_tmp`, `reads_clock`, `cpu_count`, `stat_only`. Fail
  with `PermissionDenied`: `reads_fixture`, `net_loopback`, `runs_bin`.
  `reads_fixture` passes once the fixture directory is allowed.
  Per process (`pure_add`): plain 2.45 ms, Landlock + seccomp 3.43 ms, bwrap
  12.5 ms, strace 12.9 ms, strace `--seccomp-bpf` 10.6 ms.
- **E5** Overhead on work. `file_heavy` (15,051 path calls): plain
  0.40-0.55 s, Landlock 0.41-0.46 s, strace `--seccomp-bpf` 0.99-1.00 s,
  strace 3.1-3.7 s. `cpu_heavy`: 61-69 ms plain, 70-110 ms traced.
  sha256 of 100 MB: 80 ms.
- **E6** Prototype runner (shell): key = sha256 of the binary and arguments;
  miss runs under the E4 sandbox; a sandboxed failure is repeated plain and
  remembered. Run 1: 7 stored, 3 fell back. Run 2: 7 replayed, 3 run. After
  editing one `app` unit test: `dep` and `app::it` replayed, `app` unit
  tests ran. After changing only the fixture: `reads_fixture` ran and
  failed. `stat_only` was stored as a pass: the metadata hole of R3.

Project data (read-only):

- FairFill: `~/data/fairfill/test4/` (`README.md`, `gate-ab/`,
  `gate-edit-ab/`, `edit-ab/`, `handoff-check.log`); run counts from all
  `*.log` under `~/data/fairfill*` newer than 2026-11-14.
- Longbow: `worker-01:~/data/longbow/benchmarks/TEST-TURNAROUND-20261121/`
  (`REPORT.md`, `failures.md`, `workspace-historical-rank.tsv`,
  `gate-historical-stage-times.tsv`, `candidate-stage-times.tsv`);
  `~/data/longbow/build-bench/final-remap-*/steps.tsv`.
- Markets-rs: `worker-02:~/data/markets-rs/test-turnaround/` (`README.md`,
  `workspace-baseline-872482b5f.junit.xml`, `r2/gate-tmpfs.junit.xml`);
  `~/data/markets-rs/build-bench/contract-20261120T215545Z/steps.tsv`,
  `scenario.json`.
- Graphs and histories: `cargo metadata --no-deps --offline` and `git log` in
  each project's main checkout (FairFill `9b034e2601`, Longbow `e9f43626f`,
  Markets-rs `5838dd4cf`).
- Host facts, all three hosts: `landlock` in `/sys/kernel/security/lsm`,
  Landlock ABI 8, `kernel.yama.ptrace_scope = 1` (children can be traced),
  `kernel.apparmor_restrict_unprivileged_userns = 1`. `bwrap` is present on
  devbox only.
