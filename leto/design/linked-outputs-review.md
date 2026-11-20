# Design review: caching rustc compiles that link

Date 2026-11-20. Branch `leto` at `7ff33a23`. rustc/cargo 1.99.1, x86_64 Linux.
Scope: `--test` harnesses, `bin` crates (examples, build scripts), `proc-macro`
crates; `cdylib`/`dylib` only where cheap. Read-only review plus experiments
with plain rustc/cargo (appendix). No code was changed.

Source references: `src/...` is this repository. `rust:` is rust-lang/rust at
tag 1.99.1 (`58cf8f9`), paths under `compiler/`. `cargo:` is the checkout in
`~/.opensrc/repos/github.com/rust-lang/cargo/master`, paths under `src/`
(upstream `src/cargo/core/`).

## Verdict

| Design | Verdict | Note |
|---|---|---|
| (e) cache metadata-only compiles of test/bin/proc-macro units (`cargo check`) | **safe** | No linker runs. Not in the brief; found in the data. Build first. |
| (c) cache proc-macro dylibs | **safe with conditions** | Needs the linker-reported input manifest and the linker identity in the key. |
| (a) cache whole linked outputs (tests, bins) | **safe with conditions** | Same conditions, plus env-dep values kept verbatim. Largest win. |
| (b) cache the compile part, always link | **unsafe on stable; not worth building** | Needs `-Z no-link`/`-Z link-only`; the `.rlink` file holds absolute paths. Keeps all link CPU and all disk. |
| (d) filesystem dedupe after link | **safe, disk only** | No time win, btrfs/xfs only. A complement for the units (a) cannot share. |

A key made only of what sccache hashes today (arguments, sources, direct
externs) is **unsafe** for linked outputs: experiment E8 changes a C file in a
transitive dependency and the harness bytes change while every hashed input
stays the same.

## Q1. Where and why sccache refuses today

All in `parse_argument_iter`, `src/compiler/rust.rs`:

| Line | Refusal | Hits |
|---|---|---|
| 1831-1846 | `cannot_cache!("crate-type", others)`: any `--crate-type` other than `lib`/`rlib`/`staticlib` | `bin`, `proc-macro`, `cdylib`, `dylib`, also when the invocation only emits metadata |
| 1962-1971 | `cannot_cache!("crate-type", NO_CRATE_TYPE_PASSED)`: no `--crate-type` at all | every `--test` unit: cargo omits `--crate-type` for tests (`cargo: compiler/mod.rs:1368-1373`, `--test` at 1493) |
| 1904-1910 | `"multiple input files"` | not a cargo case |
| 1825-1828 | `"more than one --emit"` | not a cargo case |
| 1974-1976 | `"unsupported --emit"` (`ALLOWED_EMIT` = link, metadata, dep-info, line 442) | `--emit=obj`, `asm`, ... |
| 1803-1805 | `TooHardFlag`/`TooHardPath`: `-`, `--sysroot`, `-o` (`ARGS`, 1491, 1513, 1527) | `-o` rules out hand-written rustc calls |
| 1852, 1857, 1890 | `extra-filename` without value, `incremental` when the mode is off, `target unsure` | |
| 1978-1991 | incremental limits (crate name, grouped flags, unpacked split debuginfo, save-temps) | |

`--test` itself is not in `ARGS`; it is an `UnknownFlag` and stays in the
hashed command line. There is no explicit "linking" refusal: linking is
refused through the crate type only. So a `--test --emit=dep-info,metadata`
compile is refused although it never links.

Why, upstream:

- The comment at 1836-1837 (commit `9657de42a`, 2017): "rustc invokes the
  system linker to link them, and we don't know about all the linker inputs".
- `docs/Rust.md:13` and `README.md:404` state it as a caveat and suggest a thin
  `bin` over a `lib`.
- mozilla/sccache#2533 "Caching rust-lld" (open): asks to cache links now that
  rust-lld is the default linker on x86_64 Linux (since 1.90). One supporting
  comment (proc-macros), no maintainer answer.
- mozilla/sccache#2897 (merged) only split the statistic into
  `crate-type (bin)` etc. (`src/server.rs:2244`).
- `gh search` for "proc-macro", "crate-type", "cdylib", "bin crates", "linker
  inputs" finds no upstream PR that tries to cache linked outputs.

The reason is completeness of the key, not nondeterminism. E4 shows the three
linkers here are deterministic.

## Q2. What determines a linked output

rustc builds the link line in `linker_with_args`
(`rust: rustc_codegen_ssa/src/back/link.rs:2896`), in this order (comment at
2970-3000): CRT objects, a generated `symbols.o`, the crate's own objects,
local native libraries, **every upstream Rust crate** (`add_upstream_rust_crates`,
3517-3590, loops over `crate_info.used_crates`), upstream native libraries, then
order-independent options (3220-3350). E2 prints the real line with
`--print link-args`.

| Input | Where it enters | sccache today | Proposal |
|---|---|---|---|
| Sources, `include_*!` files | dep-info | hashed (2609-2622, 2757) | keep |
| `env!`/`option_env!` values | dep-info `# env-dep` | hashed, basedir-stripped unless listed verbatim (2770-2779) | linked outputs: never strip (Q4.1) |
| Direct `--extern` rlib/rmeta | args | file contents hashed (2553-2569); dylibs by `.rustc` only (`src/util.rs:471-503`) | keep in the pre-key |
| **Transitive rlibs** | found through `-L dependency=` by name and SVH; cargo passes only direct externs (E2: `libnat` linked, not in `--extern`) | **not hashed** (`-L` is dropped from the key, 2704-2710) | manifest, by content |
| Native static libs bundled in an rlib (`cargo:rustc-link-lib=static=`) | inside the dependency's rlib | covered only for direct externs | manifest (the rlib) |
| `-l` on the command line | args | name hashed as text; contents only for `-l static=` found in `-L native=` (1809-1813, 2020-2040) | manifest |
| `#[link]` and `-l` of dependencies | rlib metadata, re-emitted on the final link line (`add_native_libs_from_crate`, 3377) | not hashed | manifest |
| `-L native=`/`-L all=` | args; cargo passes every build script's search path to all dependents (`cargo: compiler/mod.rs:582-646`) | dropped from the key | manifest (resolved files) + directory check |
| `-C link-arg(s)`, build-script `rustc-link-arg*` | args (UNTRACKED in rustc, `rust: rustc_session/src/options.rs:2253-2255`) | hashed as text | keep; files they name come through the manifest |
| Linker driver `-C linker`, `-fuse-ld`, self-contained rust-lld | args; resolved through `PATH` with the sysroot `bin` dirs first (E2) | name only | linker identity in the pre-key |
| Linker default search path, libc, libgcc_s, CRT objects, linker scripts | resolved by `cc`/`ld` | **unknown** | manifest: the linker reports them (E3) |
| Sysroot std rlibs | sysroot target lib dir (`walk_native_lib_search_dirs`, 144-193; link line in E2) | implied by `compiler_shlibs_digests` + version | manifest (free once memoized) |
| `-C prefer-dynamic`, PIE, relro, strip, `-C rpath`, target | args; target JSON hashed (2584-2596) | hashed as text | keep; refuse `-C rpath` |
| Env the linker reads | inherited; rustc adds `LC_ALL=C` and `PATH` (`link_natively`, 1222-1270) | not hashed | allow-list in the pre-key |
| Debuginfo paths | codegen of this crate and of every rlib | cwd hashed basedir-stripped (2811-2814) | same policy as rlibs (Q4.4) |
| `-C split-debuginfo`, dwp | extra output files | refused only for incremental | refuse unless `off` |

What is impossible to know before the link: which files the linker driver opens
(search order, linker scripts such as `libc.so`, CRT objects, `libgcc`). It need
not be predicted. All three linkers write the list on request:
`-Wl,--dependency-file=FILE` (E3). rust-lld 23.1.1, mold 2.40.4 and GNU ld 2.46
each list the objects, all 25 rlibs, `Scrt1.o`, `crti.o`, `crtbeginS.o`,
`libz.so`, `libgcc_s.so(.1)`, `libgcc.a`, `libc.so` and what that script pulls
in (`libc.so.6`, `libc_nonshared.a`, `ld-linux-x86-64.so.2`). The option does
not change the output bytes (E3b).

`-Z binary-dep-depinfo` (with `RUSTC_BOOTSTRAP=1`, as sccache already does for
`-Z ls`, `src/compiler/rust.rs:3836-3840`) would list the transitive crates in
the existing dep-info pre-pass (E2b). It is not needed if the manifest is used,
and it does not cover native or system libraries.

### Proposed key: a pre-key plus a verified manifest

Same shape as the C preprocessor cache (`src/compiler/preprocessor_cache.rs`).

**Pre-key K1** = today's Rust key (2668-2820) plus:

1. A domain separator `linked` and the output kind.
2. Linker identity: digest of the driver binary resolved as rustc resolves it
   (sysroot `bin`, `bin/self-contained`, then `PATH`); digest of the linker it
   selects (`<driver> -fuse-ld=X -print-prog-name=ld.X`, or the sysroot
   `rust-lld`); the text of `<driver> ... -Wl,--version`. Memoize per
   (path, size, mtime, ctime, inode).
3. Linker environment allow-list, verbatim: `LIBRARY_PATH`, `LD_RUN_PATH`,
   `COMPILER_PATH`, `GCC_EXEC_PREFIX`, `SOURCE_DATE_EPOCH`, `SDKROOT`,
   `MACOSX_DEPLOYMENT_TARGET`, `CCC_OVERRIDE_OPTIONS`, `LDEMULATION`, and any
   `MOLD_*`, `LLD_*`.
4. Env-deps unstripped (Q4.1).

**Manifest**, written on a miss from the linker's dependency file: for each
input, a root (basedir, sysroot, absolute), the relative path and a content
digest. The crate's own `*.rcgu.o` and rustc's temp dir are left out. Also the
linker's `-L` directories with their mtime.

**Lookup**: read the manifests stored under K1 (keep the newest 4). One matches
if every listed file still has the recorded digest and no listed directory
changed. Then K2 = hash(K1, manifest) names the stored outputs. No match: run
rustc, store a new manifest and entry.

Why this is complete: K1 pins the compile (sources, direct externs, flags) and,
through the direct externs' metadata, the name and SVH of every transitive
crate (the SVH includes all upstream SVHs, `rust: rustc_middle/src/hir/map.rs:1160-1203,
1235-1247`). The manifest pins the bytes of everything the linker read. A file
that newly shadows a library changes a directory mtime and forces a miss.

Cost: one small object read and N `stat` calls per hit once digests are
memoized (N = 48 in the toy case, a few hundred for a real harness). Hashing
all rlibs of a worktree once is a few seconds of CPU (FairFill: 3.4 GB).

Residual risk: inputs the linker does not report (linker plugins, `LD_PRELOAD`
into the linker) and a stat-keyed digest memo. Refuse `-C linker-plugin-lto`;
do not memoize files modified in the last two seconds.

## Q3. Alternatives

### (e) Metadata-only compiles of non-lib units: safe

`cargo check --all-targets` compiles tests, benches, bins and examples with
`--emit=dep-info,metadata` (`cargo: compiler/mod.rs:1376-1377`). No linker and
no codegen run. The output is a **0-byte** `lib<name>-<hash>.rmeta` for tests and
bins, and a small metadata file for a proc-macro (E1). sccache refuses them
only because of the crate-type check. These are 800 of the 1,690 not-cacheable
invocations in a Markets-rs clone and 274 of 872 in FairFill (Q5).

- Pros: no new key inputs; the existing key is complete. Nothing to link.
- Cons: `--print file-names` prints the executable name for these units, and
  `generate_hash_key` then keeps no output (2833-2864). The output name must be
  derived as `lib{crate}{extra-filename}.rmeta`.
- Risk: low. The cached result is "it type-checks" plus the diagnostics.

### (c) Proc-macro dylibs: safe with conditions

- A proc-macro is linked like any dylib, so it needs K1 + manifest.
- Registry proc-macros compile in the registry directory with no debuginfo, so
  the output is identical in every worktree already.
- Workspace proc-macros: the `.rustc` section holds absolute paths and differs
  between checkouts without path remapping (E5: same section size, different
  digest). Dependents key on that section, so today every dependent of a
  workspace proc-macro misses in a new worktree. Restoring A's dylib in B
  makes those dependents hit. This is a second win of (c).
- Pre-existing gap, upstream since mozilla/sccache#2882: dependents hash only
  `.rustc`. E8 changes C code linked into the proc-macro: the dylib changes,
  `.rustc` does not, and cargo rebuilds a dependent that sccache would serve
  from cache. With (c) on, B holds A's exact file, so dependents can hash the
  whole dylib on ELF targets and the gap closes. Needs a hit-rate check first.
- Risk: a wrong proc-macro corrupts every dependent, so this stage depends on
  the manifest being right. It is also the smallest stage (24-51 units).

### (a) Whole linked output: safe with conditions

- Pros: removes harness codegen and the link, and with reflink hits also the
  disk. Linking is deterministic: same inputs give the same bytes with lld,
  mold and bfd, and with remapped paths the bytes are equal across checkout
  directories (E4). The outputs do not embed their own path (E4, E5).
- Cons: entries are 20-200 MB instead of 1-5 MB; the LRU needs a larger limit
  or separate accounting. On ext4 (devbox) a hit is a copy.
- Conditions: K1 + manifest; verbatim env-deps (Q4.1); the refusals in Q6;
  never distributed.

### (b) Compile part only: unsafe on stable, low value

What rustc 1.99.1 offers (E6):

- `--emit=obj` (stable) writes one `.o` with the crate's code. It lacks the
  allocator shim object and the generated `symbols.o`, and there is no stable
  way to ask rustc to link a given object. Reproducing the link line by hand
  means copying `linker_with_args`.
- `-C save-temps` keeps the `.rcgu.o` files. Nothing reads them back.
- `-Z no-link` writes the objects and a `.rlink` file; `-Z link-only x.rlink`
  links them. Both work on 1.99.1 with `RUSTC_BOOTSTRAP=1` and give the same
  bytes as a one-step build. The `.rlink` file is a rustc-internal encoding and
  contains the absolute paths of the objects and of every dependency rlib, so
  it cannot move to another worktree without rewriting an unstable format.
- Cargo offers nothing: one rustc call per unit, `--emit=dep-info,link`.

Value: with mold the link is most of the CPU of a small harness (Q5:
`lake_changes` 1.4 s compile, 6.2 s link CPU) and all of the disk. (b) keeps
both. Do not build.

### (d) Filesystem dedupe after link: safe, disk only

`duperemove`/`bees` on btrfs share equal blocks after the fact. Without path
remapping the binaries of two worktrees differ in a few places (build id,
compile directory); T3 worktree paths have equal length, so most blocks still
line up. It costs a full link and write first, then a hashing pass. No effect
on devbox (ext4). Useful later for the binaries (a) cannot share.

## Q4. Correctness hazards

1. **Absolute paths baked through `env!`.** A test binary is executed, so a
   baked path is behaviour. `CARGO_BIN_EXE_<name>`
   (`cargo: compiler/artifact.rs:22-46`) and `CARGO_TARGET_TMPDIR`
   (`cargo: compiler/mod.rs:900`) are absolute paths into `target/`. Today they
   are basedir-stripped unless listed in `SCCACHE_BASEDIRS_VERBATIM_ENV_DEPS`
   (2771-2777), and Leto lists only `CARGO_MANIFEST_DIR,CARGO_MANIFEST_PATH`. A
   cross-worktree hit would give B a test that runs **A's binary**. Rule for
   linked outputs: hash every env-dep value verbatim. Seen in dep-info:
   8 Markets-rs and 6 FairFill executables use `CARGO_BIN_EXE_*`.
2. **`include_bytes!`/`include_str!` outside the crate**: listed in dep-info and
   hashed by content. Files read by a proc-macro without tracking are a
   pre-existing gap, the same as for rlibs.
3. **`option_env!`, build-script `rerun-if-env-changed`**: `option_env!` is an
   env-dep. Build-script outputs reach rustc as arguments, `rustc-env` values
   and `-L`/`-l`, all covered by K1 or the manifest.
4. **Debuginfo paths.** Without `SCCACHE_BASEDIRS_REMAP_PATHS` the binary keeps
   the compile directory of the checkout that built it (E4: one occurrence,
   `DW_AT_comp_dir`). `file!()` and panic locations are relative for workspace
   crates. This is the policy already accepted for rlibs. With remapping the
   bytes are equal across directories.
5. **Linker nondeterminism**: none seen (E4). It would only cost hits; nothing
   keys on an executable's bytes.
6. **`-C instrument-coverage`, sanitizers, PGO, `-C rpath`, linker-plugin LTO,
   split debuginfo**: coverage maps hold absolute source paths; the others add
   inputs or output files outside this design. Refuse.
7. **Fixture paths read at run time**: not compiled in, no cache problem.
   `env!("CARGO_MANIFEST_DIR")` is compiled in: 197 of 654 Markets-rs and 34 of
   206 FairFill executables use it and will not hit across worktrees. Cargo
   sets the variable when it runs tests, so `std::env::var` at run time is a
   project-side fix.
8. **Cargo's `deps/<name>-<hash>` and uplift.** Cargo hard-links the executable
   to `target/debug/<name>` after rustc (`cargo: compiler/mod.rs:679-717`). A
   hit must create a new inode (layer 3 rule R2, already implemented); writing
   through the old one would change the uplifted copy or fail with `ETXTBSY`.
9. **Incremental.** Harnesses of workspace members carry `-C incremental`. The
   layer 3 rules apply unchanged: look up only when the unit has no session,
   store only a build that used none. `metadata_output()` returns `None` for
   these units (2066-2075), so "first build is plain" needs the executable's
   existence as its test.
10. **A transitive rlib rebuilt locally with other bytes** (same SVH): the
    manifest does not match, B links and stores a second manifest. Correct.

## Q5. Expected win

Method: `not_cached` counts from `stats/clone-b-*.json`, matched one to one
against `cargo --unit-graph` for the same two commands; real rustc command
lines captured from a live Markets-rs build and re-run with `-Z time-passes`
and `-Z no-link`/`-Z link-only` into a temp directory (E7).

Not-cacheable invocations in clone B:

| | Markets-rs (`l6cmp-v6`) | FairFill (`fork-l5-pc`) |
|---|---|---|
| Total | 1,690 | 872 |
| Metadata-only test/bin/example/cdylib checks, no link | 800 | 274 |
| Test harnesses, linked | 650 | 204 |
| Bins and examples, linked | 148 | 63 |
| Build scripts, linked | 52 | 59 |
| Proc-macros | 24 | 51 |
| cdylib, linked | 1 | 1 |
| Not Rust (`-MM`, `-E`, `-`, ...) | 15 | 220 |

Markets-rs clone B: 319 s wall, 7,224 CPU-s. `test --no-run` is 6,793 CPU-s
(94%); its 18 cache misses are 279 s of compiler time, so about 6,000-6,500
CPU-s (83-90% of the clone) are the 799 linked units. The host was overloaded
(load 364 on 96 CPUs), so use CPU, not wall.

Per unit, measured (mold, worker-02):

| Unit | Compile CPU | Link wall | Link CPU | Output |
|---|---|---|---|---|
| `markets_parquet` unit tests (opt-level 1) | 89.6 s | 0.35 s | | 63 MB |
| `marketlab_datasets` unit tests | 73.1 s | 0.57 s | | 137 MB |
| `markets_databento` unit tests | 36.0 s | 0.31 s | | 76 MB |
| `lake_changes` integration test | 1.4 s | 0.53 s | 6.2 s mold, 1.2 s lld | 118 MB |
| `rr` bin | 6.0 s | 0.71 s | 10.4 s mold, 2.4 s lld | 206 MB |

The mean executable is about 106 MB, so the 799 links cost roughly 4,000-4,800
CPU-s with mold (about 1,200 with lld) and the harness compiles the remaining
1,500-2,500 CPU-s. The link CPU comes from two samples (mold with `--no-fork`,
so its child process is counted); treat the split as an estimate.

Share of clone-B that each design removes:

| Design | Markets-rs | FairFill |
|---|---|---|
| (a) tests + bins | about 57-62% of CPU now (69% of executables can hit); up to 83-90% if `env!("CARGO_MANIFEST_DIR")` moves to run time | 81% of executables can hit; `test --no-run` 108 s → about 40-60 s, bounded by its 47 misses (373 s compiler time). Low confidence, no CPU data |
| (b) compile only | about 14-25% of CPU with mold; not buildable on stable | smaller |
| (c) proc-macros | small in CPU (not measured per unit), on the critical path of `check`; plus hits for dependents of workspace proc-macros | 51 units, same |
| (e) metadata-only | 800 of the 894 rustc runs that execute in `check` | 274 of the 431 rustc runs that execute in `check` (54 s) |
| (c) + (e) together | at most 6% of CPU: the whole `check` step is 65 s wall, 431 CPU-s | most of the 54 s `check` step, bounded by its 47 misses (135 s compiler time) |

Limits of the data: `l6cmp-v6` ended `test --no-run` with exit 101 in both
clones, but all 799 linked units appear in its statistics. The env-dep counts
come from dep-info in `markets-rs-fork6-b/target` and in FairFill's main
`target/`, which can hold units from other commands.

Disk: (a) with reflink hits shares the linked outputs that hit. Markets-rs
clone B: 103 GB, 83% linked → about 45 GB. No disk win on ext4.

## Q6. Recommendation

Build (e), then (c), then (a), behind one cumulative switch, default `off`:

`SCCACHE_RUST_CACHE_LINKED=off|check|proc-macro|tests|all`
(config `[cache] rust_cache_linked`, documented in `docs/Rust.md` and
`docs/Configuration.md`). `all` adds bins, examples and build scripts. cdylib
and dylib stay refused: one unit per project.

| Stage | Adds | Status |
|---|---|---|
| `check` | metadata-only units of any crate type | safe now |
| `proc-macro` | K1 + manifest for `--crate-type proc-macro` | safe once the manifest tests pass |
| `tests` | `--test --emit=...link` | needs a soak: compare restored and rebuilt binaries on both projects, check cache size |
| `all` | `bin` | after `tests`; build scripts influence every crate |

Key: Q2. Bump `CACHE_VERSION` (446).

Refuse linked caching (compile as today) when:

- the linker wrote no dependency file, or the linker flavor is not GNU-like
  (MSVC, ld64, wasm);
- `-C rpath`, `-C linker-plugin-lto`, `-C lto` (v1), `-C instrument-coverage`,
  `-C profile-generate`/`-C profile-use`, `-Z sanitizer`, `-C link-dead-code`;
- `-C split-debuginfo` is not `off`;
- more than one crate type, or `cdylib`/`dylib`;
- the unit has a finalized incremental session (existing rule).

Never send a linked compile to a dist server.

Open before `tests`:

1. LRU accounting for large entries; whether to cap entry size.
2. The stat-keyed digest memo: confirm no stale digest under concurrent cargo
   runs in one worktree.
3. Whole-file hashing of proc-macro externs on ELF, to close the `.rustc` gap.
4. For 0-byte-rmeta units, `CARGO_MANIFEST_DIR` could be stripped (nothing is
   embedded); 201 Markets-rs check units would then hit. A const-evaluated use
   of the path's value is the counter-case. Leave off.

### Test plan

Unit tests, `mod test` in `src/compiler/rust.rs`:

1. Parsing per stage: `--test`, `bin`, `proc-macro` × metadata-only/link ×
   each switch value gives `Ok` or the existing `CannotCache` reason.
2. Each refusal above stays `CannotCache` at `all`.
3. Output of a metadata-only `--test`/`bin` unit is `lib<name><extra>.rmeta`.
4. K1 changes with: linker driver digest, linker version text, each allow-listed
   env var, a `-C link-arg`, `--test` on/off. It does not change with the
   checkout directory.
5. Env-dep values are not stripped for linked outputs; they still are for rlibs.
6. Manifest: parse the three linkers' dependency files; drop own objects and
   the temp dir; roots map a path to another basedir; a changed digest, a
   missing file and a changed directory mtime each reject it.

End to end, `tests/sccache_cargo.rs` (sandboxed server), two checkouts with
per-checkout basedirs, each for lld and mold where installed:

1. `check --all-targets` in A then B: test and bin checks hit in B.
2. `test --no-run` in A then B: harnesses hit; B's tests pass when run.
3. Proc-macro hit in B, and a dependent of a workspace proc-macro hits.
4. **Native lib changes under the same path** (E8: edit the C file of a
   transitive `-sys` crate): the harness misses and prints the new value.
5. **Linker upgrade**: a wrapper script as `-C linker` whose bytes change →
   miss.
6. **Changed link-arg** (`-C link-arg=-Wl,--build-id=none`) → miss.
7. A library added earlier in an `-L native=` directory → miss.
8. `LIBRARY_PATH` set → miss.
9. `env!("CARGO_BIN_EXE_x")` and `env!("CARGO_MANIFEST_DIR")` in a test → miss
   in B; the test runs B's binary.
10. Hit while the old binary runs and with an uplifted hard link: new inode,
    the uplifted copy unchanged until cargo relinks it.
11. A unit with a finalized incremental session bypasses the cache.
12. A linker without `--dependency-file` (stub) → compiled, not stored.
13. Restored and locally linked binaries are byte-equal with
    `SCCACHE_BASEDIRS_REMAP_PATHS=true`.

## Appendix: experiments

Toy workspace in the scratchpad: `nat` (build script compiles `c/natc.c` into
`libnatc.a`, `rustc-link-lib=static=natc`, `rustc-link-lib=z`), `mid` → `nat`,
proc-macro `pm` → `mid` (expands to `mid::answer()`), `app` (lib, bin, unit and
integration tests) → `mid`, `pm`. rustc 1.99.1, `RUSTC_WRAPPER` empty.

- **E1** Metadata-only outputs. `--test`, `--crate-type bin`:
  `lib<name>-<hash>.rmeta` of 0 bytes; proc-macro: 1,256 bytes.
  `--print file-names` prints the executable name.
- **E2** `--print link-args` for the integration test: `cc -m64`, `symbols.o`,
  two `.rcgu.o`, `libapp`, `libmid`, **`libnat`** (not an `--extern`), 21
  sysroot rlibs, `-lz -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`,
  `-B<sysroot>/.../gcc-ld -fuse-ld=lld`, `-L <nat OUT_DIR>`,
  `-L <sysroot target lib>`, `-pie`, `-Wl,-z,relro,-z,now`, `-nodefaultlibs`;
  env `LC_ALL=C` and `PATH` with the sysroot `bin` dirs first.
- **E2b** `--emit=dep-info -Z binary-dep-depinfo` lists the sysroot crates,
  `libapp`, `libpm`, `libmid` and the transitive `libnat`.
- **E3** `-C link-arg=-Wl,--dependency-file=F` with rust-lld 23.1.1, mold
  2.40.4, GNU ld 2.46: 48, 48 and 54 paths, 25 rlibs each, plus the system
  files named in Q2. **E3b** the output is byte-equal with and without it.
- **E4** Determinism, `CARGO_INCREMENTAL=0`, per linker. Same directory, clean
  rebuild: all executables and the proc-macro dylib equal. Two directories, no
  remap: all differ. Two directories with `--remap-path-prefix=<dir>=/sccache/basedir`:
  all equal except the unit-test binary that bakes `env!("CARGO_MANIFEST_DIR")`.
  No `RPATH`, `RUNPATH` or `SONAME`.
- **E5** Proc-macro dylib in two directories, no remap: `.text`, `.rodata`,
  `.data.rel.ro`, `.eh_frame`, `.dynstr` equal; build id and `.rustc` differ
  (1,272 bytes each). With remap the files are equal.
- **E6** `--emit=obj`: one 21 KB `.o`. `-Z no-link`: two `.rcgu.o` and an
  11 KB `.rlink` with 9 absolute paths; `-Z link-only` output equals the
  one-step output.
- **E7** Real units, worker-02 (table in Q5).
- **E8** Change only `nat/c/natc.c`. `libmid.rlib` unchanged (`3fb19f08`),
  `libnat.rlib` changed, the harness whose only extern is `mid` changed
  (`56f52925` → `a09348e7`). `libpm.so` changed (`2409e147` → `4e2b8568`) with
  an identical `.rustc` (`f5687c64`), and `libapp.rlib`, which uses the macro,
  changed.
