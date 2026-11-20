# Layer 3 implementation review: `t3/cache-rust-incremental`

Reviewed 2026-11-20. Spec: `leto/design/layer3-incremental-review.md` (R1-R10,
test plan) plus the parent decisions on `SCCACHE_RUST_INCREMENTAL`.

- Code lines are cited at `db63f550` (branch head; `9989c038` is the commit
  below it). The worktree had uncommitted work in progress for the private
  folder design while I read it; section 2 covers that design and names the
  WIP where I looked at it. WIP lines can move.
- Nothing was built and no sccache test was run. Everything about sccache is
  from reading. The rustc statements in section 2 come from direct `rustc`
  runs (1.99.1 and nightly-2026-09-01, no cargo, no sccache), script and
  outputs in
  `/home/leovigna/.cache/t3-agent-tmp/claude-1000/-home-leovigna-workspaces-leto-labs-sccache/5d4fe90a-e712-45a5-a0b0-4bc7bf38f669/scratchpad/pexp/`
  (`run.sh`, `work/`; session-specific). cargo is cited at
  `~/.opensrc/repos/github.com/rust-lang/cargo/master`.

## Verdict

**Merge after fixes.** The committed R5 logic (marker + observation) is a
blocker for effectiveness, not for correctness, and is being replaced. The
private folder design that replaces it is sound against rustc and cargo; I
found no way for it to store a warm output or to change an output. It needs
one operational fix outside this repo (the sweep, P1) before it is switched on
for the lanes. The rest of the branch (R1-R4, R6, R7, the key, the R2 change)
is correct; the remaining items are should-fix and nits.

| # | Severity | Where | What |
|---|---|---|---|
| B1 | blocker (being replaced) | `src/compiler/rust/incremental.rs:149-171` | A lib and its test harness finish in each other's window, so nothing is stored |
| P1 | should-fix before rollout | `core/ops/cleanup/cargo-target-sweep:40-45` | The sweep deletes an active private folder every 2 days |
| P2 | should-fix | WIP `incremental.rs` | Tests for the new design are missing (list in 2.6) |
| S1 | should-fix | `src/server.rs:1908` | `ServerStats` field added in the middle breaks `--show-stats` and `--stop-server` between versions, both directions |
| S2 | should-fix | `src/cache/cache_io.rs:165-173`, `:398`, `:427` | The stream path is kept but its test was deleted; the unwritable case is skipped as root |
| S3 | should-fix for upstream | `src/cache/cache_io.rs:183`, `src/compiler/compiler.rs:714-718` | The in-place fallback is gone for every compiler, not only Rust |
| S4 | should-fix | `docs/Rust.md` (Incremental compilation) | Describes marker files; must describe the private folder |
| N1-N8 | nit | see section 4 | |

## 1. The committed R5 logic (marker + observation)

**B1, blocker, confirmed by reading.** `observe` stores only when exactly one
`<crate_name>-*` folder gained a finalized session
(`incremental.rs:149-163`). The "before" snapshot is taken in
`prepare_cache_store` (`src/compiler/rust.rs:1684-1690`), before the command
waits for a jobserver token and spawns rustc
(`src/mock_command.rs:320-325`), so the window is the whole compile plus the
queue time. cargo runs a lib and its unit-test harness (same crate name, other
`StableCrateId`) at the same time for `check --all-targets` and `test`. The
harness is uncacheable, so it has no window of its own, but it finalizes
inside the lib's window whenever it finishes first, and for `check` both take
the same time. Result: two changed folders, no store, no marker. The marker is
also not written then, so `cache` mode never reaches the bypass for those
units.

Why no test caught it: the fixture (`tests/sccache_cargo.rs:662-703`) never
builds a lib together with its harness. Every test uses `build` or `check`
without `--all-targets`.

For the record, the classification itself had no false "cold" in any cargo
scenario I went through (check/build/test units, pre-rollout sessions,
`cargo clean -p`, a marker naming a deleted folder, rustc's own GC, two
worktrees). Two theoretical holes existed, and the first one survives in the
new design (N1):

- a second process finalizes a session in the same crate folder between the
  snapshot and rustc's session lookup. cargo's build-directory lock prevents
  it; a build system without that lock does not;
- the compile's own session is not finalized (rustc warns and exits 0) while
  another same-name unit finalizes a cold one. Gone with the new design.

No further time spent here, as asked.

## 2. The private folder design (P)

P = `DIR/<crate_name>-sccache-<hash of unit identity>`, passed to the real
compile as `-C incremental=P`. rustc then uses `P/<crate_name>-<id>/s-…`.

### 2.1 Does the path change outputs, stderr or session validity? No.

Confidence: high on Linux, not tested elsewhere.

- Outputs: cold compiles with `-C incremental=inc`, `=inc/a-sccache-…`
  (relative, missing) and the absolute form gave a byte-identical `.rmeta`,
  identical object contents in the rlib and identical stderr (with a warning
  in the source). `-C debuginfo=2`; the folder name does not occur in the
  rlib, the `.rmeta` or the `.d` file.
- rustc creates the missing nested folder itself. The crate folder name is the
  same in both layouts (`a-3etlk8e411843`), so the id does not depend on the
  path.
- Validity: `incremental` is `[UNTRACKED]`, so it is not in the session's
  command-line hash. A session moved from `ninc/a-<id>` to `ninc/P/a-<id>`
  was reused (`-Zincremental-info`: "12 files hard-linked") and the result
  equals a cold build. Sessions hold no path to themselves.
- The hard link of F2 exists in the nested layout too (`nlink=2`, same inode
  as `deps/lib*.rmeta`), so R2 stays necessary. P is under `DIR`, so it is on
  the same filesystem.
- A warm compile in P gives the same `.rmeta` as a cold one.
- stderr can name P only in rustc's I/O errors about the session folder. Such
  a compile fails, so nothing is stored.
- Not verified: Windows. From memory, LLVM writes the rustc command line into
  CodeView build info for MSVC targets with debuginfo. If so, the path is in
  the objects there, but that is already true for `DIR`, `--out-dir` and `-L`,
  and the key ignores the folder either way.

### 2.2 Is "P has no finalized session" a sound test for cold? Yes.

rustc reads sessions only from `<-C incremental>/<crate_name>-<id>/`. With an
empty P it has nothing to reuse, whatever exists in `DIR`. Verified: with a
live session in `ninc/a-<id>`, a compile with `ninc/P` linked no files and
started from scratch. A P that holds only a `-working` folder (killed compile)
also compiles cold and rustc leaves the foreign folder alone.

Soundness does not depend on the identity hash. If two units ever shared P,
the second would look warm: fewer stores, never a wrong one. The identity only
has to keep units that run together apart, and it does: `--test` separates the
harness, and cargo gives every unit its own `-C metadata`.

rustc keeps two finalized sessions in steady state, not one: the process
cannot take the lock of the session it just finalized, so the previous one is
collected on the next compile. The check must be "any finalized session", not
"exactly one". The WIP does that.

### 2.3 cargo

- `cargo clean -p`: removes every entry of `incremental/` whose name starts
  with `<crate_name>-` (`cargo:src/ops/cargo_clean.rs:336-340`, `:448-450`),
  files and folders, recursively. P matches, and so do the old flat folders.
  Crate names have no `-`, and the branch refuses other names
  (`rust.rs:1907-1909`), so the prefix cannot catch another crate.
- cargo reads nothing in `incremental/` for fingerprints. A hit after
  `cargo clean -p`, or a P that vanished, is the normal first-build case.
- P cannot collide with a rustc folder: `<crate>-<base36 id>` has one `-`,
  P has two.

### 2.4 Cleanup: P1, should-fix before rollout

`cargo-target-sweep` deletes each direct child of `incremental/` whose mtime
is older than 2 days (`core/ops/cleanup/cargo-target-sweep:40-45`). Today that
child is `<crate>-<id>`, and every compile creates and removes entries in it,
so its mtime follows use. With P the direct child is P, and its mtime changes
only when `P/<crate>-<id>` is created, that is once. Two days after its first
compile every P looks stale and is removed while in use (unless a build is
running in that checkout at sweep time).

Effect: no wrong output. The next compile is cold, is looked up and stored.
But every edited crate goes cold every 2 days, which is the cost Layer 3 is
meant to remove.

Fix, one of:

- in the sweep, for `*-sccache-*` entries use the newest mtime one level down;
- or sccache sets P's mtime after each compile it runs with it (best effort;
  `File::set_modified`, std 1.75). This also keeps other mtime-based cleaners
  working, so I prefer it.

`worktree-target-reaper` removes whole target folders and is not affected.
rustc's GC works inside `P/<crate>-<id>` only, as before; it never removes a
crate folder or P, which is the same as today for `<crate>-<id>`.

### 2.5 Rollout and mode changes

- **Sessions from before the rollout** stay in `DIR/<crate>-<id>` and are not
  used by rewritten compiles. Each unit compiles cold once per worktree; that
  compile is looked up first and stored, so it also seeds the cache. The old
  folders are orphaned and the sweep removes them after 2 days.
- **`off` → `cache`**: same as the rollout. **`cache` → `off`**: rustc goes
  back to the flat folder (a stale session there is validated by rustc), P
  stays. **Back to `cache`**: P holds a stale finalized session, so the unit
  is warm: bypassed or not stored. Correct, because rustc validates the
  session against the dep-graph. `cache` ↔ `cache-no-bypass`: same P, nothing
  to do.
- **Wrapped and unwrapped builds on one target folder** (`leto/heavy.sh`
  unsets `RUSTC_WRAPPER`; a client that falls back to running rustc itself)
  keep two session families. Both are valid. Cost: disk, and one cold compile
  per family.
- **Uncacheable units** (bin, proc-macro, test harness) are not rewritten and
  stay flat. That is consistent per unit and needs no change.
- **Pilot server next to the shared one**: a worktree that alternates between
  an `off` server and a `cache` server alternates between the two families.
  Correct; the timing comparison must allow for the cold-once effect.
- **Marker files left by `db63f550`** (the bench checkouts have them) are
  regular files named `<crate>-sccache-<16 hex>`. rustc fails hard if P is a
  file: `could not create incremental compilation crate directory …: Not a
  directory`. The WIP adds a domain tag to the hash, so the names differ. Keep
  that, or remove a non-directory at P before the compile.

### 2.6 The WIP, briefly (it will move)

Looked at the uncommitted diff of `incremental.rs`, `rust.rs`, `compiler.rs`,
`server.rs`. It follows the design:

- Bypass runs on the server with the rewritten command
  (`CompileResult::CacheBypass`), not as `CannotCache`. This matters: a
  `CannotCache` makes the client run the original command, which would use the
  flat folder. No wire change. Fine.
- Every `-C incremental` argument is rewritten, and the preflights drop all of
  them. Fine (rustc takes the last one).
- An I/O error in the cold check vetoes the store. Fine.
- Nits: the check counts only real directories; count any `s-*` entry that
  does not end in `-working` or `.lock`, so a symlinked session is not read as
  cold. Several new lines exceed 100 columns; run `cargo fmt -- --check`.

**P2, should-fix: tests the new design needs.**

- `cargo check --all-targets` and `cargo test --no-run` in checkout A, then a
  build in B: workspace libs are hits. This is the case that failed.
- A flat session from before (build with the server in `off`, restart in
  `cache`): the next compile is a miss, stored, and P exists.
- Bypass under `cargo build`, not only `check`, with the session in P reused
  (session name changes inside P, no new folder next to it).
- `cargo clean -p` removes P (adapt the existing check at
  `tests/sccache_cargo.rs:1175-1186`).
- Unit: cold check with only `-working`, with two finalized sessions, with a
  missing P, with P as a file.
- `test_incremental_preflights_share_dep_info_key` asserts the compile gets
  `incremental=b` (`rust.rs:5231`); it must assert the rewritten path.

## 3. The rest of the branch

### 3.1 R2, `src/cache/cache_io.rs`

Every path gives a new inode or writes nothing. Checked:

- Regular outputs: temp file in the destination folder, then rename
  (`cache_io.rs:183-190`). No in-place path is left.
- `/dev/null` (and `NUL*` on Windows): skipped, as before.
- `/dev/stdout`, `/dev/stderr`, `/dev/fd/N`: opened without truncation,
  written only if the handle is a FIFO or a character device
  (`:202-223`). A descriptor that points at a regular file is refused. Good.
- Errors: any extract error is now a miss followed by a local compile
  (`src/compiler/compiler.rs:714-718`), not a failed build. Outputs replaced
  before the error are overwritten by that compile. Good.
- cfgs: the stream code is `#[cfg(unix)]`; Windows and macOS keep null-skip
  plus rename. The imports are used on every platform. I see nothing that
  fails to compile; not built.
- The dep-info file is still written in place (`rust.rs:2537`). rustc does not
  link it into the session, so this is fine.

**S2, should-fix.** The upstream test that extracts to a pipe through
`/dev/fd/N` was replaced by one that expects an error for a regular file
(`cache_io.rs:398`). The pipe path still exists and now has no test. Restore
the pipe test next to the new one. Also, the "unwritable directory" case
(`:427`, `:444-452`) returns early when the process can still create a file,
which is the case as root, and the sandbox can run tests under
`sudo unshare`. There the fallback removal is not tested at all. Make the
failure independent of permissions (for example a destination whose parent is
a regular file) or assert the skip is not taken in CI.

**S3, should-fix for upstream, nit for Leto.** The fallback is removed for
all languages. Upstream added it on purpose and tested it
(`test_extract_object_to_non_writable_path`, deleted here): a C output in a
folder where no temp file can be created, or a character device other than
the three handled, was a hit and is now a miss on every compile, each followed
by a cache write. The parent asked for "every path", so this is a decision,
not a bug; say so in the PR description, or pass a flag from the Rust side and
keep the old behavior for the other compilers.

### 3.2 Key, `src/compiler/rust.rs`

**Non-incremental keys are unchanged.** Proof by the three places that touch
the hash:

- `rust.rs:2187` drops `CodeGen` arguments with `opt == "incremental"` from
  `os_string_arguments`. An argument like that reaches `generate_hash_key`
  only with a mode other than `off` and a value (`:1782-1785`), and then
  `incremental_dir` is set. For any other compile the filter removes nothing
  and the order is untouched.
- `:2395-2398` adds the marker only if `incremental_dir` is set. It is a
  `\0SCCACHE\0…\0` delimiter after the argument hash; the next input is a hex
  digest, so no non-incremental stream can equal it.
- `:2450` skips the three variables only if `incremental_dir` is set.

`CACHE_VERSION` is untouched, and the existing test with a hand-computed
digest still covers the old stream.

- **R1**: one filter feeds the dep-info run, `--print file-names` and
  `dep_info_key` (`:2182-2217`); the real compile takes
  `parsed_args.arguments` (`:2580`). All four spellings parse to the same
  `CodeGen` value (`:1495`, `:1518`). A clustered flag such as
  `-gCincremental=x` is refused (`:1841-1855`). Complete.
- **R3**: done as specified.
- **R4**: scope is right (incremental compiles only, env-deps listed in
  dep-info are still hashed). `CARGO_PROFILE_*_INCREMENTAL` matches by prefix
  and suffix; no other profile variable ends that way.
- **R6**: `:2706`. **R7**: `:1906-1919`, with `-g`, `-C debuginfo=0|none` and
  `-C save-temps`. Both fine.
- The folder is resolved against the compile's working directory (`:1783`),
  not the server's. Good.

### 3.3 Bypass and stats (R9, R10)

In the commit the bypass is a `CannotCache("incremental session")` at parse
time (`rust.rs:1004-1011`): no preflight, no lookup, no store, counted under
not-cached reasons. Placement is right for the marker design. With P it must
not be `CannotCache` (see 2.6).

`rust_incremental_not_stored` is counted on the veto (`server.rs:1714-1719`).
Incremental hits and misses are not counted apart from Rust hits and misses,
so R10 is partly done (N5).

### 3.4 Wire format

Frames are length-prefixed bincode 1, read with `bincode::deserialize`, which
ignores trailing bytes.

- `Request` and `Response` have no new variant, and `Compile` is unchanged.
  The compile path works between an old client and a new server and the other
  way round.
- `StorageHandshakeInfo.rust_incremental` is the last field
  (`src/protocol.rs:129`). Old client, new server: fine. New client, old
  server: the handshake fails to decode, so `--show-stats` fails and a
  client-side-mode compile fails (`src/commands.rs:746`). Acceptable if the
  server is restarted with the binary; say so in the rollout notes.
- **S1, should-fix.** `rust_incremental_not_stored` sits in the middle of
  `ServerStats` (`src/server.rs:1908`). `ServerStats` is in
  `Response::Stats`, `Response::ShuttingDown` and `Request::RecordStats`. With
  mixed versions every later field shifts: `--show-stats` and `--stop-server`
  print garbage or fail to decode, in both directions. The pilot compares a
  new server with the shared old one, so this will be hit. Move the field to
  the end of the struct: an old client then reads a new server correctly. A
  new client against an old server still fails; nothing short of a version
  field fixes that.

### 3.5 Tests

Tested as the plan asks, and not vacuous:

- Item 1 (`rust.rs:3879-3894`), 2 (`:5142`), 3 and 4 (`:4681`, `:4707`,
  `:5180`; the asserts rely on arguments being normalized to the separated
  form, which they are), 5 (`:5233-5244`, `dist-client` only), 6 (`:3897`).
- Item 8 (`cache_io.rs:427`): the old code fails the unwritable case, because
  it writes through the link. See S2 for the root skip.
- Item 9 / R1 (`tests/sccache_cargo.rs:866`): asserts that B has no folder at
  all for `la` and `mylib`, which is stronger than "no `s-*-working`" and
  fails without R1. `incremental_sessions` also checks it on the miss path.
- Item 11 (`:951`): asserts the output got a new inode, the session file kept
  inode and bytes, the link count dropped by one, and the later warm build
  equals a cold one in a third checkout. An in-place write fails it. It
  passes on the old code too, because the main path already renamed; the unit
  test is the one that covers the removed fallback.
- Item 12 (`:1013`), 13 (`:1069`), 14 (`:1108` and
  `tests/sccache_rustc.rs:232`), 15 (`:892`, including the third checkout
  that must miss and the fourth that must hit), 17 (`:845`, `:866`), 18
  (`:1144`).
- `incremental_cargo` sets `SCCACHE_RUST_INCREMENTAL=off` on the client
  (`:731`), so the tests also show that the server's mode wins.

Missing: item 16 (killed compile), item 19 (nightly `-Zincremental-verify-ich`
run), a lib built together with its harness (B1), bypass under `build`.
Item 7 does not apply. `assert_no_working_sessions` with `itoa` (`:859`,
`:1079`, `:1120`) checks nothing, because a registry crate is never
incremental; harmless.

### 3.6 Code quality and MSRV

- Comments are short and say why. No narration found.
- MSRV 1.91: newest APIs used are `Option::is_none_or` (1.82), let chains
  (1.88), `OsStr::as_encoded_bytes` (1.74), `Result::is_ok_and` (1.70).
  Nothing newer.
- No new dependency; `walkdir` (used by the WIP) is already in
  `[dependencies]`. `Cargo.lock` is untouched.
- `rust.rs:2187` is 121 columns inside a closure; rustfmt may leave it, but
  check with `cargo fmt -- --check`.

## 4. Nits

- **N1.** Residual race in the cold check: a second compile of the same unit
  that finalizes in P between the check and rustc's session lookup makes a
  warm compile look cold. cargo's build-directory lock excludes it. To close
  it for other build systems, hold a file lock next to P from the check to
  the end of the compile.
- **N2.** `off` is not identical to upstream in two places: the client no
  longer exits when `CARGO_INCREMENTAL=1` is set (`src/commands.rs`, block
  removed after `:996`; intended by R4, and `SCCACHE_ALLOW_INCREMENTAL` is now
  ignored without a message), and clustered `-…Cincremental=` flags are now
  `CannotCache` in every mode (`rust.rs:1849-1851`; upstream would cache
  them as non-incremental, so this is the safer side). Mention both in the PR.
- **N3.** The cold check and the old snapshot do blocking directory reads on a
  runtime thread (`compiler.rs:912-925`). Small folders; acceptable.
- **N4.** macOS: cargo passes `-C split-debuginfo=unpacked` for the dev
  profile, so R7 makes every incremental dev compile uncacheable there. Not a
  Leto concern (the synced profile has no split debuginfo and uses
  `line-tables-only`); upstream will ask.
- **N5.** R10: add separate counters for incremental hits and misses if the
  Disk Cleanup report needs them.
- **N6.** A lane that sets `CARGO_PROFILE_DEV_INCREMENTAL` in the environment
  (the bench harness does, `core/ops/cluster/build-bench/bench.sh:153`) does
  not share registry crates with a lane that uses the config file, because
  the variable stays in non-incremental keys. Documented in `docs/Rust.md`;
  keep the lanes on the config file.
- **N7.** `docs/Rust.md`: the changed `--emit` line is a correction of the
  docs to what the code already did at `leto`, not a behavior change. Fine,
  but it is unrelated to this PR's title.
- **N8.** Windows: P adds up to `len(crate name) + 25` characters to session
  paths. rustc canonicalizes the folder to a long-path form, so I expect no
  problem; not tested.

## 5. Docs (S4, should-fix)

`docs/Rust.md` says "sccache records observed sessions in small marker
files". Replace that with the private folder, and say that the first compile
of each unit after the mode is switched on is cold.
