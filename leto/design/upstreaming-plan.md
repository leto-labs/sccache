# Upstreaming the Leto sccache fork

This plan covers the 55 commits in `1616af1b..leto`, ending at `888b0a6a`
(`leto-0.20.0-1`). GitHub checked on 2026-11-21. Mozilla's `main` was
`1616af1b` at the time. The series has 25 proposed changes, including three
updates to open PRs and one conditional reflink proposal. Numbers below are
plan positions. Independent work can proceed in parallel.

The fork reviews record the experiments and earlier designs.
[Incremental design](layer3-incremental-review.md),
[incremental implementation](layer3-implementation-review.md),
[reflinks](layer4-reflink-design.md), and
[linked Rust outputs](linked-outputs-review.md).

## Inventory

**A** is a standalone upstream candidate, **B** depends on another change in
the series, **C** stays in the fork for now, and **D** is already upstream
or has been superseded there. Each row has one primary classification.
The D portions of mixed commits are identified in the overlap section.

| Commit | Subject | Class | Disposition |
| --- | --- | :---: | --- |
| `ad1fd5c6` | basedirs: take SCCACHE_BASEDIRS from each compile's environment | A | §3 |
| `d572e4d2` | config: reject a SCCACHE_BASEDIRS that is not valid UTF-8 | B | §3 |
| `58bf1201` | leto: run the test suite in a private network namespace | C | Sandbox |
| `7f385238` | leto: queue builds and tests on the build-bench timing lock | C | Benchmark locking |
| `24b8757c` | preprocessor cache: treat an unreadable entry as a miss | A | §2 |
| `5a1cc396` | preprocessor cache: re-root recorded includes at the tree being compiled | A | §2 |
| `10c3f546` | docs: state what the preprocessor cache does not check across basedirs | B | §2 |
| `ebc7fbdb` | c: use compile request basedirs for direct mode | B | §3 |
| `4baedd76` | rust: remap basedirs in rustc outputs with SCCACHE_BASEDIRS_REMAP_PATHS | A | §4 |
| `a3de6850` | rust: share the `..` path of the basedir remapping | B | §4 |
| `48b237eb` | tests: cover cross-checkout cache hits with compile basedirs | B | §2–4, split fixtures |
| `43e7daa2` | leto: layer 3 design review (incremental-aware caching) | C | Design review |
| `9bd75500` | leto: layer 4.2 reflink design draft | C | Design draft |
| `d7767cbb` | leto: sandbox tests with sudo unshare where bwrap cannot run | C | Sandbox |
| `1d5e200e` | leto: fork builds and tests no longer wait for build-bench timing runs | C | Benchmark locking and implementation review |
| `e68926e4` | cache: replace extracted files without truncating hard links | A | §1 |
| `4a649c2e` | rust: cache cold incremental compiles | B | §9 (upstream core is D) |
| `84e7b903` | rust: isolate incremental sessions by compilation unit | B | §9 |
| `c384a1d1` | cache: use fresh inodes when temporary restore fails | B | §1 |
| `25276b2f` | protocol: preserve legacy stats and handshake frames | B | §9 and later protocol changes |
| `b416297f` | rust: refresh active incremental session directories | B | §9 |
| `c2197978` | disk: serve cache hits with reflinks | B | §14 |
| `f03fa0c9` | rust: bypass warm sessions locally and defer cache packaging | B | §10–11, split |
| `06e75d6e` | leto: linked outputs on btrfs, measured per linker; recommend lld | C | Workload measurements |
| `b08ab46d` | leto: pre-warm the shared Rust cache | C | Pre-warm |
| `ce719e1c` | leto: run pre-warm compiles client-side | C | Pre-warm |
| `3429ec4b` | rust: explain cross-worktree cache misses | A | §13 (script stays in fork) |
| `db279e59` | disk: weigh reflink cache entries by estimated disk use | B | §16 |
| `a117bab9` | rust: prune stale incremental session directories | C | Host-specific session pruning |
| `43759406` | rust: compile first incremental cache misses without a session | B | §12 |
| `7d9d2801` | client: fall back to local compilation on disconnect | A | §6 |
| `1d3c3a18` | prewarm: gate on spare CPU and available memory | C | Pre-warm |
| `c6c91d5a` | rust: plain first builds skip thin local LTO like the incremental compile they replace | B | §12 with LTO rule reused in §25 |
| `05f95649` | ipc: negotiate raw disk cache entries | B | §15 |
| `2324d527` | disk: reflink compiler outputs into cache entries | B | §17 |
| `1c0f4902` | test: use standard vec macro delimiters | C | Test style cleanup |
| `535dd8cd` | leto: design review of caching linked Rust outputs | C | Design review |
| `c91571b7` | server: drain requests during managed restarts | B | §7–8, split |
| `5badf692` | nvcc: format normalized filenames in test | C | NVCC formatting cleanup |
| `5999452a` | rust: cache metadata-only check targets | A | §18 |
| `d47e73ee` | rust: cache linked proc-macros with verified inputs | B | §19–21, split |
| `9b464f4a` | rust: preserve failed linked compiler output | B | §20 |
| `a41fe181` | rust: cache linked test harnesses and executables | B | §22–23, split |
| `7f7aab68` | rust: exercise linked executable caching with Cargo | B | §22–23, split tests |
| `a234e903` | leto: compare cached and rebuilt test harnesses | C | Soak script and usage |
| `411ea8b6` | server: verify startup with linked Rust cache stages | B | §18, §20, §22–23 (stage tests) |
| `436a3344` | leto: soak driver for the linked-output stages on one host | C | Soak driver |
| `84107411` | leto: soak driver keeps the stats on a difference; --remap option | C | Soak driver |
| `016e0d7d` | rust: cache Cargo targets without an explicit crate type | B | §24 |
| `aef2fff1` | rust: use plain first builds for local uncached compiles | B | §25 |
| `c4d53d4a` | rust: skip Cargo-internal variables in semantic cache keys | A | §5 (upstream exclusions are D) |
| `295e97d0` | rust: explain cache misses by compiler argument | B | §13 |
| `84d0f575` | leto: extend linked soak to stage all | C | Soak extension and usage |
| `21a61285` | leto: sandbox test temp dirs live next to the repo | C | Sandbox |
| `888b0a6a` | leto: handle Cargo build-script artifact variants | C | Soak fixtures and artifact comparison |

## Proposed PRs in merge order

Commit lists refer to source changes to extract or adapt. The tests described
as carried are present on the fork. Each PR needs to run its own relevant
unit and integration tests after rebasing. Fork settings are listed where
they differ from the proposal.

### 1. cache: restore outputs without writing through hard links

rustc hard-links output metadata into its local incremental session. If the
cache restore cannot create a temporary file, truncating the old output can
overwrite the metadata rustc later reuses. Apply the fresh-inode rule to
upstream's current incremental caching and to future clone paths.

- **Commits and dependencies** — `e68926e4`, `c384a1d1`. Independent.
- **Tests** — Cache I/O hard-link/inode, exclusive-create, and pipe regressions.
  Extract the Cargo edit/revert/edit test from `4a649c2e` and run it with
  upstream's opt-in. Force the failure deterministically under root and add
  a Windows alias/restore case.
- **Config** — None.
- **Review** — Prefer rename. If a regular output cannot be safely replaced
  with a new inode, recompile. Review the fork's unlink-plus-exclusive-create
  fallback for partial writes and races. Preserve null sinks, FIFO and
  character-device behavior. This also protects Cargo's executable hard links.

### 2. preprocessor cache: validate includes in the requesting checkout

Direct mode can validate recorded headers against the checkout that wrote
the entry and return its object to a checkout with different headers.
Re-root includes under the requesting source tree and pin entries that
refer to another configured basedir to their producing tree.

- **Commits and dependencies** — `24b8757c`, `5a1cc396`, `10c3f546` and
  startup-basedir portions of `48b237eb`. Independent. Update
  [open #2868](https://github.com/mozilla/sccache/pull/2868), which fixes
  [#2863](https://github.com/mozilla/sccache/issues/2863).
- **Tests** — Carry unreadable-entry miss, same/different header, external
  include, independently rooted include and C system cases. Check GCC and
  Clang, plus Windows/MSVC path and line-marker behavior on the rebase.
- **Config** — Existing `basedirs` and `SCCACHE_BASEDIRS`.
- **Review** — Adapt to upstream's current preprocessor storage and byte
  paths. The entry version bump makes old entries miss and rebuild. Document
  the remaining header-shadowing limitation when a new file was absent from
  the recorded includes.

### 3. basedirs: take SCCACHE_BASEDIRS from each compile's environment

A single daemon's startup roots cannot accommodate newly created worktrees
without a restart. Resolve the roots per request for both hash computation
and C direct mode, preserving the startup fallback.

- **Commits and dependencies** — `ad1fd5c6`, `d572e4d2`, `ebc7fbdb`, and
  request-scoped portions of `48b237eb`. After §2. Update
  [open #2984](https://github.com/mozilla/sccache/pull/2984) for
  [#2595](https://github.com/mozilla/sccache/issues/2595). If
  [open #2981](https://github.com/mozilla/sccache/pull/2981) lands first,
  propose only the remaining startup-fallback and direct-mode behavior.
- **Tests** — Carry resolver and UTF-8 cases, Cargo in daemon and client modes,
  and GCC/Clang fixtures. Add concurrent requests with different roots if
  the rebased suite lacks them. Check empty override and invalid fallback
  end to end.
- **Config** — Existing `SCCACHE_BASEDIRS` and top-level `basedirs`. Request
  value takes precedence, empty clears it, and invalid warns and falls back.
  Invalid startup input fails startup.
- **Review** — #2981 intentionally ignores the server startup environment.
  Agree on precedence and validate the same effective root through every
  compiler path before expanding use of direct mode.

### 4. rust: remap basedirs in rustc outputs

Cache hits across worktrees can restore absolute paths into Rust metadata and
debug info. Opt-in remapping can stabilize those outputs while allowing
explicit user mappings to take precedence.

- **Commits and dependencies** — `4baedd76`, `a3de6850` and remapping portions
  of `48b237eb`. Update [open #3021](https://github.com/mozilla/sccache/pull/3021)
  after §3 in this plan. The core can land independently with startup roots.
  See [#2765](https://github.com/mozilla/sccache/issues/2765).
- **Tests** — Carry remap and key tests, the `..` path case and cross-worktree
  Cargo with an external path dependency and build script. Verify disabled
  keys, Windows case handling, user override, and no dist command.
- **Config** — Keep per-compile `SCCACHE_BASEDIRS_REMAP_PATHS` (off by default).
  Reuse upstream's separate `SCCACHE_BASEDIRS_VERBATIM_ENV_DEPS` setting.
- **Review** — Preserve recognizable diagnostic paths. Document debugger
  working directory, synthetic mappings for external output roots, and local
  execution.

### 5. rust: ignore Cargo settings already represented in compiler inputs

Cargo network, terminal and other internal controls can split Rust cache
keys even when compiler inputs are identical. Extend the exclusions already
in upstream and continue to hash every explicit dep-info environment read.

- **Commits and dependencies** — `c4d53d4a` on top of upstream `03377355`.
  Independent.
- **Tests** — Carry same-key cases for known internals and different-key cases
  for package metadata, unknown variables and explicit dep-info reads.
  Carry the two-checkout test in both execution modes. Add unset-vs-empty
  and profile/rustflags tests that also change the resulting rustc arguments.
- **Config** — Fork `SCCACHE_RUST_HASH_CARGO_ENV=semantic|all` and top-level
  `rust_hash_cargo_env`, default `semantic`. Propose a small reviewed
  expansion of upstream's filter with no new knob. Preserve upstream's broad
  `CARGO_BUILD_*` exclusion.
- **Review** — Fork `semantic` still hashes unknown `CARGO_BUILD_*` names to
  preserve Leto 0.19 keys, whereas `all` excludes them like upstream. Keep
  that compatibility policy in Leto. Check implicit compiler and proc-macro
  inputs category by category.

### 6. client: retry a compile locally after a server disconnect

A daemon restart can interrupt a compile request or the client-side storage
handshake. A local retry can let the build continue through transport
disconnects while preserving compiler errors and exit status.

- **Commits and dependencies** — `7d9d2801`. Independent. Carry the drain
  timeout integration case from `c91571b7` in §7.
- **Tests** — Carry mock no-response, truncated response, reset, handshake
  and actual compiler-error cases. Add client-side storage disconnect and
  retry after an artifact notification, including competing output writers.
- **Config** — Fork automatically retries selected connection errors and
  retains `SCCACHE_IGNORE_SERVER_IO_ERROR=1` for broader errors. Start upstream
  review with the existing opt-in and document eligible phases before
  considering a change to default behavior.
- **Review** — Preserve cancellation and jobserver behavior. Ensure old and
  retried rustc processes cannot race on outputs or repeat observable build
  effects. Related [#2761](https://github.com/mozilla/sccache/pull/2761) and
  [#2779](https://github.com/mozilla/sccache/pull/2779).

### 7. server: drain active compiles and cache writes on shutdown

Stopping a shared daemon during a build can cut off established requests and
pending writes. Close the listener first, allow a bounded drain on
`--stop-server`, SIGTERM or SIGINT on Unix, and allow a second SIGTERM to end
the wait.

- **Commits and dependencies** — Drain parts of `c91571b7`. After §6 to retain
  fallback on short drains. Land with the pending-write tracking needed by
  this slice, and include §11 in the shared lifecycle tests when it lands.
- **Tests** — Carry signal and explicit stop, short drain, repeated SIGTERM
  and deferred-write tests. Check a busy idle-timeout transition and real
  multilevel writes before claiming coverage of
  [#2819](https://github.com/mozilla/sccache/issues/2819).
- **Config** — Keep `SCCACHE_SHUTDOWN_DRAIN_SECS` and
  `[server] shutdown_drain_secs` (10 seconds, 0 skips the wait).
- **Review** — Preserve Windows explicit shutdown and the current-thread
  runtime. Fork enables Tokio's `signal` feature with no lockfile change.
  Treat detached multilevel storage tasks as a separate concern unless
  covered. Related [#204](https://github.com/mozilla/sccache/issues/204).

### 8. client: allow compiles to wait for a managed server

An autostarting client can start its own daemon while a service manager is
restarting the shared one. Let managed installations disable auto-start and
retry for a bounded interval before compiling locally.

- **Commits and dependencies** — Autostart and connect-wait parts of
  `c91571b7`. After §6 and §7.
- **Tests** — Carry the no-server and managed-start tests. Add explicit
  `--start-server` with autostart disabled and a stale Unix socket case.
- **Config** — Keep `SCCACHE_SERVER_AUTOSTART` / `[server] autostart`
  (enabled by default) and `SCCACHE_SERVER_CONNECT_WAIT_SECS` /
  `[server] connect_wait_secs` (30 seconds, 0 means one attempt).
- **Review** — Check both execution modes, timeout and compiler exit
  propagation. Keep connection waiting scoped to the managed mode.

### 9. rust: store only cold incremental compiles in the shared cache

Upstream already looks up incremental invocations, but a miss can store
outputs produced from reused local state. Private per-unit folders let
sccache identify cold compiles and restrict shared writes to them while
keeping the normal local incremental rebuild path.

- **Commits and dependencies** — Cold-store delta of `4a649c2e`, final private
  directory logic from `84e7b903`, relevant compatibility work from
  `25276b2f`, and mtime refresh and daemon warm bypass from `b416297f` and
  earlier changes. After §1 and on top of `6f6e3905`.
- **Tests** — Carry private folder identity/cold detection, `cargo check
  --all-targets` and `test --no-run` same-crate harness isolation, preexisting
  flat sessions, stale sessions, edit/warm/no-store, hit/dep-info behavior,
  dist refusal, and legacy protocol tests as needed. Add killed-child,
  concurrent same-unit non-Cargo, and Windows/macOS cases. Verify
  `cargo clean -p` and rerun stale-session scenarios with nightly ICH checks.
- **Config** — Fork `SCCACHE_RUST_INCREMENTAL` and `rust_incremental` support
  `off` (default), `cache` (warm bypass), and `cache-no-bypass` (warm lookup,
  still cold-only writes). The fork accepts `SCCACHE_ALLOW_INCREMENTAL=cache`
  as a per-compile alias when the server is off. Propose extending upstream's
  existing opt-in, retaining `1` and default semantics. Use a temporary
  `cache-cold` opt-in if maintainers want to compare behavior before changing
  `cache`. Defer the fork's server mode matrix.
- **Review** — Treat ambiguous session reads as ineligible to store. Use unit
  identity to distinguish sessions of the same crate. Document one cold
  transition, private-folder mtime, `save-temps` and unpacked-debug-info
  limits, and how concurrent compiles of the same unit exclude reuse between
  detection and spawn. Keep new counters and IPC fields version-compatible.

### 10. rust: bypass the server for warm incremental sessions

Even a daemon-side bypass pays IPC and queueing overhead on every warm edit.
A client that can resolve the same private unit folder can run rustc before
opening a connection.

- **Commits and dependencies** — Client bypass parts of `f03fa0c9`. After §9.
  Reapply request-scoped roots and remap logic from §3–4 if used there.
- **Tests** — Carry the client/daemon unit-path matching and socket-free
  Cargo tests for both execution modes. Add mismatched client/server config
  and remap cases and check response-file/jobserver handling.
- **Config** — The fork needs `SCCACHE_RUST_INCREMENTAL=cache` in the build
  environment as well as server policy. Use the explicit per-invocation
  upstream selector from §9 and require enough client-side data to determine
  the same command. Keep the server path for ambiguous settings.
- **Review** — Preserve the exact rustc arguments, diagnostics and session
  folder. Document that direct client bypasses do not enter daemon stats.

### 11. cache: package outputs after reporting successful compilation

Packaging large outputs can keep rustc's caller waiting after the compiler
has finished. Open the outputs first, report completion promptly, then
package from the retained descriptors and track the pending write.

- **Commits and dependencies** — Packaging parts of `f03fa0c9`, with the
  corresponding drain test from `c91571b7`. After §7.
- **Tests** — Carry cache-entry open-inode and drain coverage. Add a test where
  Cargo replaces an output immediately after success and an overlapping
  same-key lookup waits for the pending write. Exercise client-side exits.
- **Config** — None.
- **Review** — Key ownership, error/stat reporting, bounded background work
  and shutdown lifetime. Measure latency and peak memory with large outputs.

### 12. rust: skip incremental setup on first cache misses

An incremental cache miss with no prior output or session pays the cost of
creating state before there is an edit to reuse it. Compile that first miss
plain and suppress thin local LTO with `-C lto=off` unless rustc already
has an explicit LTO policy. When an output exists, the next cache miss can
start a session.

- **Commits and dependencies** — `43759406`, `c6c91d5a`. After §9.
- **Tests** — Carry expected-`.rmeta` selection, key stability, server/client
  mode cold and warm transitions, cross-policy hits and LTO argument tests.
  Add a direct optimized build verifying explicit LTO and a dependent rebuild
  with changed metadata/SVH.
- **Config** — Fork `SCCACHE_RUST_INCREMENTAL_FIRST_BUILD=plain|incremental`
  and `rust_incremental_first_build`, default `plain` in cache modes.
  Retain names if upstream wants both policies. Start with `plain` opt-in
  under the existing incremental feature while measuring the first edit.
- **Review** — The fork shares a key for plain and cold incremental codegen
  within the incremental request domain. Rustc metadata and codegen can
  differ. Explicitly agree on that contract upstream or key the policies
  separately. Preserve already-explicit LTO and the non-incremental domain.

### 13. rust: explain cache misses by changed inputs

A Rust miss tells the user little about which input changed between worktrees.
Opt-in diagnostics can compare a miss to recent builds of the same unit and
show which sources, externs, environment entries or compiler arguments differ.

- **Commits and dependencies** — `3429ec4b`, `295e97d0`, excluding
  `leto/explain-misses.py`. Independent, with a mechanical rebase onto §5's
  filter and the current protocol.
- **Tests** — Carry unit identity, bounded history, closest-match and per-flag
  differences, and Cargo two-checkout tests in both modes. Add log permission
  and concurrent writers tests and verify enabling the log leaves keys intact.
- **Config** — Keep `SCCACHE_MISS_EXPLAIN` / `miss_explain`, an absolute JSONL
  path, off by default and empty to disable.
- **Review** — Explain argument-value redaction/digests, exposed filenames or
  other metadata, history growth and format stability. No diagnostic write
  failure should affect a compile.

### 14. disk: restore local cache artifacts with reflinks

Large cache hits duplicate output extents and spend time decompressing.
Aligned raw members allow a standalone Linux disk cache to clone into fresh
output inodes, copying tails or whole objects when cloning fails.

- **Commits and dependencies** — `c2197978`. After §1. Coordinate with
  [open #2739](https://github.com/mozilla/sccache/pull/2739). This PR slot
  represents the chosen base implementation.
- **Tests** — Carry alignment boundaries, mixed/raw/legacy entries, clone
  failure and inode preservation, the probe and multilevel compression cases,
  and Cargo two-checkout coverage. Run a Btrfs/XFS lane that asserts actual
  extent sharing and an incompatible-filesystem lane that asserts fallback.
- **Config** — Fork `SCCACHE_DISK_REFLINK` / `[cache.disk] reflink`, off.
  Prefer one upstream name, `SCCACHE_FILE_CLONE` /
  `[cache.disk] file_clone` if #2739 establishes it.
- **Review** — The fork keeps a single ZIP entry with 4 KiB-aligned `raw/`
  members and leaves diagnostics compressed. #2739 uses directory entries
  and covers more platforms. Choose one format, cover downgrade and
  compressed fallback, and preserve §1 on every platform. Keep the fork's
  raw format local to standalone disks.

### 15. ipc: negotiate raw disk cache entries with the daemon

Client-side compiles need to write the entry format the daemon expects.
Advertise the daemon's raw-object capability and use compressed entries for
older peers.

- **Commits and dependencies** — `05f95649`, using the compatibility pattern
  from `25276b2f`. After §14 and conditional on the chosen transport.
- **Tests** — Carry legacy handshake and packing tests plus the Cargo test
  in both modes. Add real old/new binary pairs for handshake, stats, stop
  and both directions of entry reads.
- **Config** — Reuse §14's setting. No new user option.
- **Review** — IPC carries entry bytes and provides no source file descriptor
  for a client-side clone hit. Keep the copy path in both modes and revisit
  the wire design if upstream chooses directory entries.

### 16. disk: account for compressed reflink entries in the cache limit

On compressed Btrfs, an uncompressed member's logical size can overstate
its expected physical cost. A persisted estimate can restore useful
capacity while retaining logical-size accounting elsewhere.

- **Commits and dependencies** — `db279e59`. After §14 and conditional on the
  chosen disk format.
- **Tests** — Carry weighted eviction, reload, length fallback, and the
  Btrfs test that keeps an evicted output usable. Add denied/malformed xattr
  cases and compare eviction and actual allocation with representative data.
- **Config** — No new sccache setting. Fork enables estimates for Btrfs with
  4 KiB sectors and a zstd compression property such as `zstd:1`.
- **Review** — Fork estimates with zstd level 1 in 128 KiB chunks and saves
  `user.sccache.weight`. Bound estimation costs, validate acceptance of xattr
  values, and document that shared extents outlive an evicted entry.

### 17. disk: clone compiler outputs into raw cache entries

Raw entries can avoid a second full data write on a daemon-side miss.
Clone from retained compiler output descriptors into staged entries and
publish atomically, falling back to packing on clone failure.

- **Commits and dependencies** — `2324d527`. After §11 and §14. Adapt §15's
  storage path when present. Drop this slot if #2739 supplies equivalent
  behavior for the chosen format.
- **Tests** — Carry packed-vs-cloned bytes and mode, copy-on-write isolation
  and forced clone failure. Run real clone tests on Btrfs and the cache
  backend build matrix after the `CacheStorage` API change.
- **Config** — Reuse §14's setting.
- **Review** — Client IPC still packages and transfers bytes. Verify source
  replacement after success, staging cleanup, read-only and multilevel
  behavior, and interaction with weighted eviction when enabled.

### 18. rust: cache metadata-only checks for non-library targets

Tests, bins and proc-macros can emit metadata without invoking a linker.
Recognize these invocations and derive the actual metadata output, which
may be an empty `.rmeta`.

- **Commits and dependencies** — `5999452a` and the matching startup tests
  from `411ea8b6`. Independent of the linker work. Rebase any incidental
  incremental or server-test dependencies out of the first patch.
- **Tests** — Carry parser and output-name cases, unsupported emit cases,
  `cargo check --all-targets` in two worktrees and both execution modes,
  and metadata-only proc-macro coverage. Compare off-stage keys and
  Windows/macOS filenames.
- **Config** — Fork starts `SCCACHE_RUST_CACHE_LINKED=off|check` /
  top-level `rust_cache_linked`. Propose enabling covered checks as normal
  Rust caching after validation. If maintainers want an opt-in first, keep
  the fork's `check` stage with no link permission.
- **Review** — Validate exactly what rustc emitted. A command that also
  emits a link output remains subject to the linking rules. Existing
  `cargo check` support from [#302](https://github.com/mozilla/sccache/pull/302)
  covers library checks.

### 19. rust: hash ELF proc-macro code when keying dependents

A native code change in a proc-macro can change its expansion while leaving
its `.rustc` section intact. A dependent may then receive an artifact built
with the old expansion. Include the ELF dylib's code in the hash, initially
by hashing the file.

- **Commits and dependencies** — ELF extern-hashing portion of `d47e73ee`.
  Adapt independently of caching proc-macro outputs. §4 helps preserve
  cross-worktree hits.
- **Tests** — Extract the native-code-dependent scenario from the linked
  proc-macro fixture and make it self-contained. Require a regression that
  proves unchanged `.rustc` bytes, changed expansion, and a dependent
  miss even when the macro itself compiled uncached. Compare warm hit rates
  with `SCCACHE_BASEDIRS` and remapping.
- **Config** — Fork gates whole-file ELF hashing on the linked cache stage.
  Propose it as an ELF correctness follow-up with no new public setting.
- **Review** — Upstream [#2882](https://github.com/mozilla/sccache/pull/2882)
  intentionally improved sharing with metadata-only hashes. Whole-file
  hashing may reduce hits when dylibs embed checkout paths or vary unrelated
  bytes. Keep non-ELF behavior until those formats are covered explicitly.

### 20. rust: cache rust-lld proc-macros after verifying linker inputs

The Rust compile key alone misses transitive native libraries and system
objects that can change a proc-macro dylib. Key the lookup on a separate
link domain, then verify a linker-reported manifest before any restore.
Start with a native Linux GNU configuration using rust-lld and a supported
driver, the smallest implementation useful to
[#2533](https://github.com/mozilla/sccache/issues/2533).

- **Commits and dependencies** — rust-lld key, capture, verification,
  proc-macro and startup portions of `d47e73ee` and `411ea8b6`, plus
  `9b464f4a`. After §1, §18 and §19. Reuse §9–10 for incremental linked
  requests if available. Keep incremental links uncached until integrated.
- **Tests** — Carry rust-lld depfile, K1 identity/environment, path and
  library-search verification, and two-worktree proc-macro/dependent tests.
  Carry the failing-link diagnostics and fail-closed depfile cases. Add
  changes during capture and stale-digest checks.
- **Config** — Fork adds `proc-macro` to the cumulative
  `SCCACHE_RUST_CACHE_LINKED` / `rust_cache_linked` setting. Reuse the
  opt-in and keep linked caching off by default.
- **Review** — K1 adds output kind, driver/linker digests and version,
  ordered search paths, flags and linker environment to ordinary Rust
  inputs. Hash dep-info env values verbatim and retain upstream's distinct
  unset value. The fork stores one manifest alongside the artifact at K1,
  verifies current sysroot/basedir/absolute paths, content digests and
  search snapshots for shadowing, and treats a mismatch as a miss. The
  design note's multi-manifest K1/K2 scheme is an alternative. Veto writes
  on incomplete reports or changing inputs. Preserve failed compiler
  output and fail closed for unsupported links. Document the trusted
  compiler, driver and linker assumptions.

### 21. rust: verify proc-macro linker inputs with GNU ld and mold

Some Linux toolchains select GNU ld or mold through a compiler driver.
Extend the verified path where those linkers produce usable dependency files
and the driver and linker identities are established.

- **Commits and dependencies** — GNU ld and mold portions and fixtures from
  `d47e73ee`. After §20.
- **Tests** — Carry both depfile fixtures, resolution/search and unsupported
  format tests, and the mold and bfd Cargo cases, including linker replacement.
  Make those jobs exercise the configured linker in CI and add a
  shadowing-library miss with each driver.
- **Config** — Reuse the `proc-macro` stage with no new option.
- **Review** — Check driver defaults and explicit `-fuse-ld`, linker
  scripts, CRT files and unsupported options. Keep uncertain toolchains
  on the uncached path.

### 22. rust: cache linked test harnesses with verified inputs

Repeated test builds in fresh worktrees relink harnesses even when the
compiler and linker inputs match. Reuse the verified proc-macro path for
test executables, preserving embedded path semantics and executable modes.

- **Commits and dependencies** — Test stage and common size-limit portions
  of `a41fe181` and `7f7aab68`, plus the matching startup case from
  `411ea8b6`. After §20 and §21, with §9–10 for incremental integration.
  The core uses §1. Reflinks are optional, with a combined test after §14.
- **Tests** — Carry Cargo unit/integration harness hits, embedded
  `CARGO_BIN_EXE_*` and manifest paths, running/uplifted inode replacement,
  cache size cap in both modes, warm bypass and concurrent digest memo.
  Compare restored and rebuilt behavior and bytes with remapping in CI.
- **Config** — Add cumulative `tests`. Keep
  `SCCACHE_RUST_CACHE_LINKED_MAX_ENTRY` /
  `rust_cache_linked_max_entry_bytes` (256 MiB on uncompressed outputs).
  Prefer `SCCACHE_RUST_CACHE_LINKED_MAX_ENTRY_BYTES` if naming afresh.
- **Review** — Keep unsupported flags and target families uncached, including
  thin archives, plugin/explicit LTO, rpath, coverage and unpacked debug
  output. Keep linking local. Test size pressure and missing depfiles.

### 23. rust: cache linked binaries, examples and build scripts

Cargo's remaining executable targets can share artifacts when the same
manifest checks hold. Restoring a build-script executable must still leave
Cargo free to run it and regenerate its own directives and files.

- **Commits and dependencies** — Executable portions of `a41fe181`,
  `7f7aab68` and the `all` startup case from `411ea8b6`. After §22.
- **Tests** — Carry Cargo bins, examples and build-script fixture, including
  artifact execution. Add a compact test for cold versus restored build
  script stdout and generated files across target layouts. Use the fork soak
  for evidence and keep its scripts out of the PR.
- **Config** — Add cumulative `all` to the linked opt-in and reuse the size
  cap. cdylib and dylib remain uncached.
- **Review** — A wrong build script can affect the whole build. Verify
  checkout-specific paths and side effects. Demonstrate benefit with the
  default linker before widening supported environments.

### 24. rust: cache Cargo targets without an explicit crate type

Custom `harness = false` targets can omit both `--crate-type` and
`--test`. Infer the applicable stage and output safely from the Cargo
invocation so these targets can use the same verified path.

- **Commits and dependencies** — `016e0d7d`. After §18, §22 and §23.
- **Tests** — Carry implicit-bin stage and filename tests and the custom
  test/bench Cargo fixture. Add cases with absent Cargo markers and
  conflicting or explicit crate types.
- **Config** — Existing stages. Metadata-only targets use `check`. Linked
  implicit binaries with `--cfg test` use `tests`, and the rest require
  `all`, including custom benches without `--cfg test`.
- **Review** — Keep inference conservative for non-Cargo invocations.
  Apply the same manifest, size cap and executable-mode checks as §22–23.

### 25. rust: skip incremental setup on first uncached builds

Binaries, examples, tests and proc-macros that fall outside caching can
still spend time creating a session on a first build. Use the same
plain-first policy when the expected output is missing, including on
an authorized local fallback. Subsequent local rebuilds can use Cargo's
original incremental directory.

- **Commits and dependencies** — `aef2fff1`, reusing `c6c91d5a`.
  After §6, §10, §12 and §24.
- **Tests** — Carry output-path and argument-spelling tests, response files,
  explicit LTO, and Cargo cold-to-warm uncached bin and custom harness cases.
  Add a disconnect case that exercises the real fallback and a custom bench.
- **Config** — Reuse the incremental mode and first-build knobs from §9 and
  §12. Keep uncached application explicitly opt-in because it can run
  with no daemon to supply policy.
- **Review** — Keep existing output or unknown layout on the incremental
  path. Check rustc output discovery, jobserver and response-file handling,
  and dependent SVH validation for proc-macros. This changes codegen even
  when caching is off for the target, so measure cold and first-edit
  tradeoffs separately.

## Upstream overlap and conflicts

| Upstream work | Fork overlap and follow-up |
| --- | --- |
| [#3020](https://github.com/mozilla/sccache/pull/3020) / [`6f6e3905`](https://github.com/mozilla/sccache/commit/6f6e3905), merged. [#236](https://github.com/mozilla/sccache/issues/236) is still open. | The duplicated portion of `4a649c2e` is D. Upstream already supports per-compile `SCCACHE_ALLOW_INCREMENTAL=cache`, ignores the directory in keys and preflights, separates incremental keys, and runs locally. Keep those semantics and upstream's key encoding where possible. §1, §9–10, §12 and §25 add independent safeguards and performance policy around it. The shared cache stores artifacts, while sessions stay local. |
| [#3050](https://github.com/mozilla/sccache/pull/3050) / [`03377355`](https://github.com/mozilla/sccache/commit/03377355), merged. | The existing `CARGO_TARGET_DIR` and `CARGO_BUILD_*` exclusion in `c4d53d4a` is D. §5 extends upstream's filter for other reviewed variables and preserves dep-info reads. Leto's two-mode key compatibility stays local. |
| [#2868](https://github.com/mozilla/sccache/pull/2868), [#2984](https://github.com/mozilla/sccache/pull/2984) and [#3021](https://github.com/mozilla/sccache/pull/3021), open and listed by GitHub as unmergeable on the checked heads. | Update the existing PRs with their authors and preserve attribution. Adapt §2 to the current byte-based preprocessor entry and storage split. §3 integrates request roots with §2, and §4 retains both remap and upstream verbatim/unset env-dep behavior. [#2981](https://github.com/mozilla/sccache/pull/2981) is open and overlaps §3. [#2814](https://github.com/mozilla/sccache/pull/2814) closed unmerged. |
| [#3022](https://github.com/mozilla/sccache/pull/3022), merged, and upstream `ee61e456`. | Preserve opt-in verbatim env-deps and unset-vs-empty key encoding in §4, §5 and §13. The linked stages always hash explicitly read values verbatim. |
| [#2882](https://github.com/mozilla/sccache/pull/2882) and [#2897](https://github.com/mozilla/sccache/pull/2897), merged. [#2533](https://github.com/mozilla/sccache/issues/2533) is still open. | #2882 hashes proc-macros by `.rustc`, which §19 corrects for the ELF native-code case. #2897 exposes non-cacheable crate types in stats. §18 and §20–24 add actual caching. A bundled linker simplifies identification while transitive inputs still require verification. |
| [#2739](https://github.com/mozilla/sccache/pull/2739), open. [#2640](https://github.com/mozilla/sccache/pull/2640) closed unmerged. [#1053](https://github.com/mozilla/sccache/issues/1053) and [#1174](https://github.com/mozilla/sccache/issues/1174) are open. | Coordinate §14–17 with #2739, which already proposes restore and store clones with a different format and an in-place fallback. Apply §1's invariant there. Reassess which IPC and accounting follow-ups remain once upstream picks a format. |
| [#2761](https://github.com/mozilla/sccache/pull/2761), merged. [#2779](https://github.com/mozilla/sccache/pull/2779) closed unmerged. [#2819](https://github.com/mozilla/sccache/issues/2819) is open. | Retain cancellation on client disconnect in §6. Discuss the existing fatal-error policy before making more errors retry by default. Verify which writes §7 drains separately from detached multilevel writes/backfills. |

No complete fork commit was already patch-equivalent to the checked main
(`git cherry 1616af1b leto`). The D dispositions here are portions of the
two mixed commits described above. Open PRs are accounted for in the series.

## What stays in the fork

- Pre-warm scripts, resource gating, timers, sandbox wrappers, benchmark
  locking and design reviews depend on Leto's host and workload setup.
- The linked soak drivers, build-script comparator and soak walkthroughs in
  `docs/Rust.md` are deployment tools. Use their results to select small
  Cargo fixtures and keep the benchmark and linker recommendations here.
- Keep `leto/explain-misses.py` as a local consumer of the upstream proposal.
  The JSONL format can stand on its own.
- Keep `a117bab9` and `SCCACHE_INCREMENTAL_PRUNE_DAYS` /
  `incremental_prune_days` (7 days by default). It can delete old flat as
  well as private folders under a Cargo target. Upstream can consider
  garbage collection after ownership and concurrency rules are agreed.
  Private-folder mtime refresh travels with §9.
- Keep the Leto 0.19 semantic key mode and the fork's server policy aliases
  unless upstream wants a migration path. Drop unrelated test and NVCC
  formatting hunks from the proposed PRs.

## Before the first PR

- The user creates `leto-labs/sccache` as a GitHub fork. The repository API
  returned 404 at review time. Check visibility and access after creation.
- Recheck open PR states and rebase candidate branches onto then-current
  upstream `main`. Coordinate the open PRs with their authors and base each
  new branch only on prerequisites that have landed.
- Enable CI on the fork. Use the MSRV from the rebased `Cargo.toml` (1.91.0
  at this baseline), preserve locked builds, and run the upstream feature
  matrix independently for `s3`, `gcs`, `azure`, `redis`, `memcached`,
  `gha`, `webdav`, `oss`, `cos` and `dist-client`. Include platform
  and filesystem lanes where the PR needs them.
- Run the required checks below. Add unit tests in the affected compiler,
  hashing, or cache module and end-to-end tests for changes to real
  compiles or server lifecycle. Record unavailable toolchains/filesystems
  and skipped tests. Justify any new dependency or dependency feature
  and keep `Cargo.lock` compatible with `--locked`.
- The author reads each diff, writes a short PR description in their own
  words, and replies to reviewers themselves. Use the upstream
  `<area>: <what changed>` title style. The statements above describe
  scope for planning.

```sh
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings -A unknown-lints \
    -A clippy::type_complexity -A clippy::new-without-default
cargo test --locked --lib --bins --tests
```

## Next steps

1. Create the fork and enable CI, then confirm the basedirs PR order with
   the authors of #2868, #2984 and #3021.
2. Prepare §1 on current main and get a deterministic failing restore
   regression against upstream incremental caching. In parallel, rebase
   §2 and extract the independently reviewable §18 check cases.
3. Agree on upstream incremental opt-in semantics and the reflink format
   before preparing their dependent patches. Gather first-build vs
   first-edit and linked soak measurements for later review.

The best value for effort is §1 for hard-link correctness, §2 for the
wrong-object fix backed by an open PR, and §18 for expanding cache coverage
without invoking a linker.
