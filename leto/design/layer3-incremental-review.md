# Layer 3 review: caching rustc compiles that use `-C incremental`

Reviewed 2026-11-20 against the proposal in
`core/docs/infrastructure/2026-11-20-rust-build-master-plan.md` ("Layer 3").

- sccache lines are cited at `leto` = `08113012`. The branch moved from
  `30a62172` during the review (Layer 2 commits landed); nothing in them
  changes a conclusion, but see 6.9.
- rustc lines are cited as `rustc:<path under compiler/>` at tag `1.99.1`
  (`58cf8f9c3`). cargo lines are cited as `cargo:<path>` at the local checkout
  `~/.opensrc/repos/github.com/rust-lang/cargo/master` (0.103.0).
- "Exp N" refers to the experiments in the appendix. All were run with real
  rustc/cargo, no sccache server, on 1.95.0, 1.97.1, 1.99.0, 1.99.1 and two
  nightlies unless a version is named.

## Verdict

**Safe with changes.** The core claim holds: rustc validates its session
against its own dep-graph and never reads the crate's own rlib/rmeta from the
target directory, and a dependent's session validates upstream crates by their
SVH. But the proposal as written has two real correctness holes and three
points where the plan's premises are wrong:

| # | Finding | Kind |
|---|---|---|
| F1 | sccache's `rustc --emit=dep-info` preflight inherits `-C incremental`. Every lookup, hits included, then creates a session directory, hard-links the previous session into it, writes a dep-graph and leaves a `s-…-working` folder behind. | Hole (breaks "hit does not touch the session") |
| F2 | rustc hard-links the output `.rmeta` into the session (`metadata.rmeta`, same inode). Any in-place write to `deps/lib*.rmeta` corrupts the session, and rustc then emits **old metadata for new source**. sccache's normal hit path renames (safe); its fallback path and a naive reflink (Layer 4) write in place. | Hole (silent wrong output, then stored in the shared cache) |
| F3 | The plan says dependents are keyed on the metadata section only. That is true only for dylibs (proc-macros). `.rmeta` and `.rlib` externs are hashed as whole files. | Wrong premise, harmless (more conservative) |
| F4 | Metadata is **not** the same with and without incremental: the SVH always differs, and above `opt-level=0` so do the inlining tables and the exported symbol set. | Answers the plan's open question: no |
| F5 | cargo gives a workspace crate a different `-C metadata`/`-C extra-filename` when incremental is off, so incremental lanes and `CARGO_INCREMENTAL=0` CI can never share a workspace-crate entry, whatever sccache does. | Wrong premise, limits the benefit |
| F6 | An output produced from a **reused** session is only as correct as rustc's incremental engine. Today such a bug stays in one `target/` and `cargo clean` fixes it; with Layer 3 it is stored and served to every worktree on the host. | Risk amplification |
| F7 | Every recompile in an edit loop now pays the dep-info preflight (26-30% of an incremental rebuild in Exp 9) plus compressing and storing an rlib that will almost never be hit. | Effectiveness (5% gate) |

Required changes are R1-R8 below.

## Q1. Cached outputs next to an incremental session

**Does rustc read the crate's own on-disk rlib/rmeta?** No. Confidence: high.

- The session lives entirely in
  `DIR/<crate>-<base36 StableCrateId>/s-<time>-<rand>-<svh>/`
  (`rustc:rustc_incremental/src/persist/fs.rs:205-290`, `:577-583`). rustc
  copies the newest finalized session into a private `-working` folder, loads
  `dep-graph.bin`, `query-cache.bin` and `work-products.bin` from there
  (`rustc:rustc_incremental/src/persist/load.rs:44-120`), and reuses a codegen
  unit only if its dep-node can be marked green **and** the saved object exists
  in the session (`rustc:rustc_codegen_ssa/src/base.rs:1238-1281`,
  `load.rs:69-86`). Objects are linked from the session into the output
  directory (`rustc:rustc_codegen_ssa/src/back/write.rs:883-960`); nothing is
  read back from the rlib.
- Exp 4 (rename case): compile v1 incrementally, replace `liba.rmeta`/`.rlib`
  with a v0 build by write-temp-then-rename (what a hit does), recompile v1:
  output is byte-identical to a from-scratch v1.

**What the session validates.** Confidence: high.

- Compiler identity: every session file starts with the rustc version string;
  a mismatch discards the file (`rustc:rustc_incremental/src/persist/file_format.rs:145-170`,
  `:190-198`). The crate folder name also contains the `StableCrateId`, which
  hashes the rustc version (`rustc:rustc_interface/src/passes.rs:952-957`), so
  two toolchains do not even share a folder.
- Command line: the hash of all tracked options; a mismatch purges the session
  (`load.rs:99-115`, `:215-223`).
- Sources: not by file hash or mtime. The dep-graph's input nodes are
  re-computed from the current source (HIR, spans, tracked env) and compared
  by fingerprint; a sample of loaded results is re-verified
  (`rustc:rustc_query_impl/src/incremental.rs:83-103`).
- Upstream crates: only through `crate_hash`. It is `eval_always`
  (`rustc:rustc_middle/src/queries.rs:2031-2035`), read from the dependency's
  metadata header (`rustc:rustc_metadata/src/rmeta/decoder/cstore_impl.rs:380`),
  and every extern query depends on it (`cstore_impl.rs:150-156`).

**Dependents compiled incrementally against a cache-served dependency.** Safe.
Confidence: high for correctness, medium for "no rustc bug hides here".

- Exp 5: one persistent session for `b`, while `a` in the same folder was
  swapped (by rename) through six variants: v0/v1 × built with/without
  incremental. Each incremental rebuild of `b` produced an `.rmeta`
  byte-identical to a from-scratch build against that `a`, and the linked
  binary printed the right values each time.
- If a dependent is ever paired with the wrong dependency, rustc refuses it:
  `E0460 found possibly newer version of crate a which b depends on` (Exp 5b).
  The failure mode is a build error, not a silent mismatch.
- The one way to defeat this is the same SVH with different metadata content.
  Layer 3 adds no such case: incremental and non-incremental builds always
  have different SVHs (Q2).

**The crate itself recompiled after a hit.** Safe if the hit did not write
through the session's hard link (F2). The session may be absent (cold
compile), old, or from another commit; red/green marking handles all three.

**Partial sessions.** Safe. Confidence: high. A killed rustc leaves only
`s-…-working`; only finalized folders are used as a source (`fs.rs:495-538`),
and later runs garbage-collect unlocked `-working` folders older than 10 s
(`fs.rs:585-587`, `:770-800`). Exp 6: `kill -9` mid-compile, then a normal
build succeeded and used a fresh session. sccache stores nothing because the
exit status is non-zero (`src/compiler/compiler.rs:795-805`).

**Locking.** Each session has its own lock file, taken without waiting
(`fs.rs:451-473`). rustc as a child of the sccache server instead of cargo
changes nothing. Concurrent rustc processes on one crate folder are part of
the design (`fs.rs:1-38`).

**F1: the preflight is not harmless.** Confidence: high (Exp 2).

`generate_hash_key` builds `filtered_arguments` by dropping only `--emit` and
`--out-dir` (`src/compiler/rust.rs:2105-2116`) and runs
`rustc … --emit dep-info -o <tmp>` with it (`rust.rs:442-475`). With
`-C incremental` in that list, rustc sets up the dep-graph before expansion
(`rustc:rustc_interface/src/passes.rs:961`), returns early for a dep-info-only
build (`rustc:rustc_driver_impl/src/lib.rs:305-309`) and never finalizes
(`lib.rs:340-342`). Measured:

- No prior session: it creates `incremental/a-<id>/s-…-working` plus a lock
  file on every run. Nothing ever collects them for a crate that is only
  served from cache, so "no incremental folders for unchanged crates" is false.
- Prior session: it hard-links all session files into a new `-working` folder,
  then purges them because `--emit` is a tracked option, then writes a new
  dep-graph. The folder stays until the next real incremental compile.
- The dep-info text is identical without `-C incremental` (Exp 2), so
  stripping it is free.

`--print file-names` (`rust.rs:793-825`) does not create anything (Exp 2).

**F2: the hard link.** Confidence: high (Exp 3, Exp 4).

With `--emit=metadata[,link]` (what cargo passes for every lib), the metadata
file rustc reports is the final `deps/lib<crate>.rmeta`
(`rustc:rustc_metadata/src/fs.rs:77-101`), and at link time rustc saves it as
the `metadata` work product by `link_or_copy`
(`rustc:rustc_interface/src/queries.rs:96-107`,
`rustc:rustc_incremental/src/persist/work_product.rs:20-56`,
`rustc:rustc_fs_util/src/lib.rs:62-85`). Result: one inode, two names
(`stat` shows `nlink=2` on all tested versions). On the next compile, if the
metadata dep-node is green, rustc does not re-encode; it links the session's
copy back out (`rustc:rustc_metadata/src/rmeta/encoder.rs:2480-2494`).

Exp 4, in-place case: after `cat v0.rmeta > deps/liba.rmeta`, the session's
`metadata.rmeta` was v0 too, and an incremental rebuild of the v1 source
emitted **v0 metadata** (both the `.rmeta` and the copy embedded in the rlib)
around v1 object code. Under Layer 3 that pair would then be stored under the
v1 key.

Where sccache stands today: the normal hit path writes a temp file in the
same folder and renames it (`src/cache/cache_io.rs:172-180`), which gives a
new inode and leaves the session intact. The fallback when the temp file
cannot be created truncates the destination in place
(`cache_io.rs:186-200`). The planned reflink hit (Layer 4) is the same hazard
if `FICLONE` targets the existing destination file. `.dwo` files are linked
the same way (`rustc:rustc_codegen_ssa/src/back/write.rs:467-507`), but sccache does not write those.

The same hard link means nothing may ever hard-link `deps/*.rmeta` *into* the
cache either: rustc replaces the name rather than rewriting the inode
(`rustc:rustc_metadata/src/fs.rs:126-130`), but the cache should not depend on that.

## Q2. Metadata identity

**How sccache keys dependents.** `hash_all_externs` hashes every `--extern`
file whole, unless its extension is the platform's dylib extension; only then
does it hash the `.rustc` section (`src/util.rs:471-503`). So `.rmeta` (what
cargo passes to pipelined lib builds, Exp 7) and `.rlib` (what it passes to
`staticlib`, tests and bins) are whole-file. The plan's sentence about
`rustc_metadata_section` applies to proc-macro dylibs only. Only direct
externs are hashed (`rust.rs:1736`, `:2139-2145`); transitive crates are
covered because a direct dependency's metadata records the SVH of each of its
own dependencies.

**Same crate, with and without `-C incremental`: metadata differs.**
Confidence: high.

- The SVH includes every owner's span only in incremental mode
  (`rustc:rustc_middle/src/hir/map.rs:1204-1217`), so the header hash differs
  for every crate.
- Above `opt-level=0`, incremental also disables cross-crate-inlining
  inference (`rustc:rustc_mir_transform/src/cross_crate_inline.rs:77-81`),
  deduced parameter attributes (`deduce_param_attrs.rs:180-182`,
  `rustc:rustc_metadata/src/rmeta/encoder.rs:1879-1892`) and the MIR inliner at
  `opt-level=3` (`inline.rs:51-56`).
- Exp 1 (1.99.1), `.rmeta` size without / with incremental: 10361 / 10376
  bytes at O0, 9944 / 9932 at O1, 16873 / 9735 at O3. At O1/O3 the
  non-incremental rlib no longer defines `a::m1::small`, `a::m1::big` or
  `<P as Tr>::go` (they are inlined into dependents); the incremental one
  does.

**Different `-C codegen-units`: metadata identical.** `codegen_units` is
`[UNTRACKED]` (`rustc:rustc_session/src/options.rs:2211`). Exp 1: `.rmeta`
byte-identical for 1, default 16 and 256 without incremental; the rlib
differs (3 vs 26 objects). `incremental` is also `[UNTRACKED]`
(`options.rs:2240`), and two incremental builds into different folders give
the same `.rmeta` (Exp 1, Exp 8).

**Consequence for sharing.** An incremental producer and a non-incremental
producer give different `.rmeta` bytes, so their dependents never share
entries. They would share the crate's own entry only if the rest of the
command line were equal, and under cargo it is not (F5, Exp 7): cargo hashes
the whole profile, `incremental` included, into `-C metadata`
(`cargo:src/workspace/profiles.rs:799-813`,
`cargo:src/compiler/build_runner/compilation_files.rs:787-791`), and sets
`incremental = false` for everything that is not a path source
(`profiles.rs:334-342`). Observed for a workspace member: `-C metadata=011ffabf…`
with incremental, `d39f00f0…` with `CARGO_INCREMENTAL=0`. Third-party crates
are never incremental, so they are shared exactly as today.

So: lanes share with lanes; a `CARGO_INCREMENTAL=0` producer shares only
third-party crates with them. A cache seeder for workspace crates must build
with the same profile as the lanes (incremental on).

**Can this cause an incorrect hit?** No. Confidence: high. A dependent's key
contains the bytes of the exact `.rmeta` it is compiled against, each entry's
rlib and rmeta come from one rustc run, and a mismatched pair is rejected by
rustc (E0460). The pre-existing assumption (same key ⇒ interchangeable
output) is not weakened, provided R3 keeps the two modes in separate entries.

## Q3. Outputs

Confidence: high for the tested flags on Linux; not tested on macOS/Windows.

- Default, `-C debuginfo=2`, `-C split-debuginfo=packed`: the output folder
  holds exactly `<crate>.d`, `lib<crate>.rlib`, `lib<crate>.rmeta` in both
  modes on every tested version (Exp 8). `--print file-names` prints only the
  rlib; sccache adds the rmeta itself (`rust.rs:2389-2409`). The set sccache
  stores is right. Extra files exist only inside the session folder
  (`<cgu>.o` per unit, `metadata.rmeta`, `dep-graph.bin`, `query-cache.bin`,
  `work-products.bin`; identical list on all versions).
- The rlib is complete: 26 objects for the test crate with incremental, 1
  without, all inside the archive. No `*.rcgu.o` stays outside for `lib` or
  `staticlib`. `bin`, `cdylib`, `dylib`, `proc-macro` remain uncacheable
  (`rust.rs:1722-1730`), as do test harnesses, which cargo builds with
  `--test` and no `--crate-type` (Exp 7; `rust.rs:1835-1841`).
- Incremental rlibs are not reproducible: each member is named
  `<crate>.<cgu>.<random per invocation>.rcgu.o`
  (`rustc:rustc_session/src/config.rs:1289-1291`,
  `rustc:rustc_interface/src/util.rs:632`). Object contents and `.rmeta` are
  identical between runs (Exp 1). Effect: anything that hashes the rlib whole
  (a cacheable `staticlib` crate, `-Zbinary-dep-depinfo`) misses after a local
  recompile. Not a correctness issue.
- `-C split-debuginfo=unpacked` with debuginfo: rustc leaves `.dwo` files
  beside the rlib (`rustc:rustc_codegen_ssa/src/back/link.rs:1944-1965`). 1
  file without incremental, 26 with, and with incremental the names carry the
  random suffix (Exp 8). sccache does not know about them, so a hit yields an
  rlib whose debug info points at files that are not there. This gap exists
  today without incremental; Layer 3 widens it and the names stop being
  predictable. Refuse it (R7).
- `-C save-temps` leaves objects, bitcode and two temp folders (Exp 8); also
  not cached today. Not Layer 3 specific.
- No output embeds the incremental folder's path: with `-C debuginfo=2` and
  two differently named folders, `.rmeta` and all objects were identical and
  the name did not occur in the rlib (Exp 8).

## Q4. Cache key

- **Drop:** the value of `-C incremental=…`, in all spellings
  (`-C incremental=X`, `-Cincremental=X`, `--codegen incremental=X`,
  `--codegen=incremental=X`). After parsing these are `("-C" | "--codegen",
  "incremental=X")` pairs in `os_string_arguments` (`rust.rs:2091-2101`).
- **Keep a marker** that incremental was on (R3). Sharing between the two
  modes would be internally consistent, but the outputs differ systematically
  (Q2), and above O0 an incremental build is slower code. A non-incremental
  compile (CI, release, a benchmark) must never receive it. Under cargo the
  marker costs nothing because the keys already differ (F5).
- **Keep everything else.** `-C codegen-units` is in the key today (all
  arguments are hashed except `--extern`, `-L`, `--check-cfg`, `--out-dir`,
  `--diagnostic-width`; `rust.rs:2268-2301`). cargo passes it only when the
  profile sets it (`cargo:src/compiler/mod.rs:1433-1435`); otherwise rustc
  picks 256 with incremental and 16 without
  (`rustc:rustc_session/src/session.rs:1066-1071`). `-Z share-generics`
  defaults from `opt-level` alone (`rustc:rustc_session/src/config.rs:1521-1529`),
  which is in the key; incremental does not change it. Keep the unstable
  incremental flags (`-Z incremental-verify-ich`, `-Z incremental-ignore-spans`,
  `-Z incremental-info`, …) in the key; two of them change the SVH or stderr.
- **Environment:** every `CARGO_*` variable is hashed
  (`rust.rs:2338-2360`). `CARGO_INCREMENTAL`, `CARGO_BUILD_INCREMENTAL` and
  `CARGO_PROFILE_*_INCREMENTAL` therefore split the key between a lane that
  sets the variable and one that uses the profile. Exclude them (R4).
- **Does the partitioning leak into dependents?** No, as far as the sources
  and experiments show. Confidence: medium-high. A dependent only references
  symbols that the dependency's metadata exports; that list is part of the
  `.rmeta` bytes in the key. Symbol names do not depend on incremental (Exp 1:
  the symbols common to both modes have identical names). The extra global
  symbols of an incremental rlib are hidden cross-unit helpers that no other
  crate names. `#[inline]` functions are instantiated in the dependent from
  MIR in both modes.

## Q5. Cargo interaction

Confidence: high.

- cargo creates `incremental/` itself (`cargo:src/compiler/layout.rs:434`) and
  only passes the path (`cargo:src/compiler/mod.rs:2016-2025`). Nothing in the
  fingerprint code reads it. The only other user is `cargo clean -p`, which
  removes `incremental/<crate_name>-*`
  (`cargo:src/ops/cargo_clean.rs:336-340`).
- A missing session folder is the normal first-build case: rustc creates it
  (`rustc:rustc_incremental/src/persist/fs.rs:216-219`). Hit, then edit, then incremental compile works (Exp 4,
  Exp 5).
- Before it runs rustc for a unit that emits `.rmeta`, cargo deletes the
  sibling `.rlib` (`cargo:src/compiler/mod.rs:448-458`); it does not delete
  the `.rmeta`. A hit therefore replaces an existing `.rmeta`, which is where
  F2 bites.
- Pipelining: rustc emits the `.rmeta` artifact notification from the same
  place in both modes (`rustc:rustc_metadata/src/fs.rs:96-98`), also when the
  metadata is reused from the session. sccache forwards it early on a miss
  (`src/compiler/compiler.rs:223-232`) and replays stderr on a hit. No
  incremental-specific behavior. Not run end to end through sccache here.
- The folder name is `<crate_name>-<id>`, and a lib and its unit-test harness
  have the same crate name with different ids (Exp 7: `la-01z1…`, `la-3je0…`).
  Any logic that looks for "the session of crate X" by name prefix is wrong.

## Q6. Anything else

1. **F6, cache poisoning by incremental bugs.** A miss compile that reuses a
   session produces an output that depends on rustc's incremental engine being
   right. rustc itself only spot-checks: one in 32 loaded results per session
   (`rustc:rustc_query_impl/src/incremental.rs:95-103`). An ICE is harmless
   (nothing stored). A silent wrong output is stored, served to every
   worktree, and survives `cargo clean`. Probability low, blast radius the
   whole host. See R5.
2. **F7, edit-loop cost.** Exp 9 (syn 2.0.119 as a workspace crate, dev
   profile, 1.99.1): incremental rebuild after a one-function edit 0.66 s,
   unchanged rebuild 0.58-0.61 s, dep-info preflight 0.17-0.19 s, plus
   zstd of a 29 MB rlib and the cache write. The preflight is re-run for
   every dependent too, because their extern hashes changed
   (`rust.rs:519-528`). Today these compiles skip all of it. Cold incremental
   build 2.59 s against 1.98 s non-incremental: the first compile of a missed
   crate is ~30% slower than the non-incremental one Longbow runs today.
3. **Cache churn.** Every edit state of every lane becomes a cache entry
   (rlib + rmeta) that is almost never hit, in a 300 GiB LRU shared with the
   useful entries. R5 removes most of it.
4. **Distributed compiles.** A `-C incremental` compile must stay local: the
   path is local and the session must be created where cargo expects it.
   `sccache-dist` also still refuses `CARGO_INCREMENTAL`
   (`src/bin/sccache-dist/main.rs:62`). Not used by Leto, needed for
   upstream (R6).
5. **Client refusal.** `sccache` exits with "incremental compilation is
   prohibited" when `CARGO_INCREMENTAL=1` or `CARGO_BUILD_INCREMENTAL=1` is in
   the environment unless `SCCACHE_ALLOW_INCREMENTAL=1`
   (`src/commands.rs:996-1011`). Must go or flip (R4).
6. **Proc-macros.** Building one stays uncacheable. Depending on one is
   unchanged: the dylib is hashed by its `.rustc` section.
   `-Z cache-proc-macros` (nightly) only acts with incremental
   (`rustc:rustc_expand/src/proc_macro.rs:113-115`) and is a tracked option,
   so it is in both the key and the session check.
7. **LTO / `-C embed-bitcode` / `-C linker-plugin-lto`.** Arguments, so in
   the key. Pre-LTO bitcode for incremental stays in the session
   (`rustc:rustc_codegen_ssa/src/back/write.rs:846-851`). For `lib` outputs
   LTO runs at the final link, which is not cached. A `staticlib` with LTO and
   incremental was not tested.
8. **`-C instrument-coverage`, `-Z incremental-verify-ich`, `RUSTC_BOOTSTRAP`,
   `-Zbinary-dep-depinfo`.** No new interaction found. The first two are
   arguments in the key. `-Zbinary-dep-depinfo` makes dependency rlibs
   "source files", which with non-reproducible incremental rlibs (Q3) means
   extra misses, not wrong hits.
9. **Layer 2 remapping.** `SCCACHE_BASEDIRS_REMAP_PATHS` (new at `0f363f86`)
   prepends `--remap-path-prefix` arguments to the real compile
   (`rust.rs:2253-2258`). That option is tracked
   (`rustc:rustc_session/src/options.rs:393`), so it is part of the session's
   command-line hash: stable per worktree, but toggling the variable or the
   basedirs makes the next compile of every crate cold. "rustc runs unchanged"
   is no longer literally true; it does not need to be.
10. **Rustc version skew.** Covered twice: sccache keys on the compiler
    (`rust.rs:2249-2251`, `:2365`), and a session from another rustc is
    discarded (Q1).
11. **Stale sessions.** A crate that is edited once and then served from
    cache keeps its last session until `cargo clean` or the reaper; rustc
    only collects when it runs for that crate. Disk only.

## Required design changes

- **R1. Strip `-C incremental` from both preflight runs and from
  `dep_info_key`.** Filter the pair out of `filtered_arguments`
  (`rust.rs:2105-2116`), out of the arguments given to `get_compiler_outputs`
  (`rust.rs:2119-2131`) and out of `dep_info_key_arguments` (`rust.rs:2124`).
  Only the real compile keeps it.
- **R2. A hit must create new inodes.** For Rust outputs, never write through
  an existing path: remove the truncate-in-place fallback
  (`cache_io.rs:186-200`) for them (treat the failure as a miss, or unlink
  first and fail if that fails). Write the same rule into the Layer 4 design:
  reflink into a temp file, then rename; never hard-link between cache and
  `deps/` in either direction.
- **R3. Replace, do not drop, the argument in the key.** Hash a constant
  `-C incremental` marker in place of `-C incremental=<dir>`
  (`rust.rs:2268-2301`). Bump `CACHE_VERSION` only if the non-incremental key
  changes; it should not.
- **R4. Environment.** Leave `CARGO_INCREMENTAL`, `CARGO_BUILD_INCREMENTAL`
  and `CARGO_PROFILE_*_INCREMENTAL` out of the key (`rust.rs:2338-2356`).
  Remove or invert the client check (`commands.rs:996-1011`) and the
  `SCCACHE_ALLOW_INCREMENTAL` documentation (`docs/Rust.md:13`,
  `docs/Configuration.md:196`, `README.md:405`).
- **R5. Do not store the output of a compile that reused a session** (default;
  an opt-out is fine). Only cold incremental compiles, which depend on no
  earlier state, feed the shared cache. This bounds F6 to what a
  non-incremental cache already trusts, and removes the per-edit churn. Cost:
  a lane's later edit states are never stored, and are compiled once more by
  the first worktree that reaches that state without a session; this matches
  today's behavior. Detection must use the exact session folder, not the
  crate-name prefix (Q5). Two workable ways:
  - compute `<crate_name>-<base36(StableCrateId)>` in sccache
    (`rustc:rustc_span/src/def_id.rs:162-190`: crate name, sorted
    `-C metadata` values, exe/lib, version string) and self-check it: after an
    incremental compile the computed folder must exist, otherwise treat the
    compile as "session reused" and log. A wrong computation then costs
    stores, never correctness;
  - or keep a marker file per unit that sccache writes after any incremental
    compile it ran, named so that `cargo clean -p` removes it
    (`incremental/<crate_name>-sccache-<hash of crate name + -C metadata>`).
    Sessions that predate the rollout look cold once.
- **R6. Never distribute an incremental compile.** Return no dist command when
  `-C incremental` is present, as is already done for remapped basedirs
  (`rust.rs:2607-2609`).
- **R7. Refuse `-C split-debuginfo=unpacked`** (with debuginfo enabled) as
  `cannot_cache!`, at least when `-C incremental` is present. Optionally also
  `-C save-temps`.
- **R8. Make it a setting.** Keep upstream's behavior (do not cache
  incremental compiles) available, e.g. `SCCACHE_RUST_INCREMENTAL=off|cache`,
  so the pilot server can be compared with the shared one and upstream can
  take the feature as opt-in. Document it in `docs/Rust.md` and
  `docs/Configuration.md`.

Recommended, not blocking:

- **R9. Skip the cache for a unit that has a session** (same detection as
  R5): no preflight, no lookup, no store, exactly today's cost. Decide after
  the P2 edit-loop measurement; without it F7 applies to every recompile.
- **R10. Count** hits, misses, "not stored: session reused" and "bypassed:
  session present" for incremental compiles separately in the stats, so the
  Disk Cleanup report can show the effect.

## Test plan

Unit tests, `mod test` in `src/compiler/rust.rs`:

1. `test_parse_arguments_incremental` (`rust.rs:3759`): now parses; with the
   setting off it still returns `CannotCache("incremental")`. Cover
   `-C incremental=x`, `-Cincremental=x`, `--codegen incremental=x`,
   `--codegen=incremental=x`.
2. Key (with the existing `hash_key` helpers): two incremental folders give
   the same key; incremental and non-incremental give different keys;
   `-C codegen-units=1` and `=256` give different keys; `CARGO_INCREMENTAL=1`
   in the environment does not change the key.
3. Preflight: with a mock creator, assert that neither the dep-info command
   nor the `--print file-names` command contains `incremental=`, and that the
   compile command does.
4. `dep_info_key` is equal for two incremental folders.
5. Dist (`dist-client` feature): `generate_compile_commands` returns no dist
   command for an incremental compile.
6. `-C split-debuginfo=unpacked -C debuginfo=2 -C incremental=x` is
   `CannotCache`.
7. Session-folder computation (if R5 uses it): fixed vectors taken from real
   rustc 1.95-1.99 folder names for a given crate name and `-C metadata`.

Unit test, `src/cache/cache_io.rs`:

8. `extract_objects` onto a path that has a second hard link: the other name
   keeps its old content and the destination has a new inode. A second case
   with an unwritable folder: the result is an error, not an in-place write.

End to end, `tests/sccache_cargo.rs` (two checkouts as in
`test_rust_cargo_basedirs_other_checkout`, cargo's default incremental dev
profile, `SCCACHE_BASEDIRS` set):

9. **New worktree hits.** Build checkout A, then B. B's workspace libs are
   hits; `B/target/debug/incremental` has no folder for them (this fails
   without R1).
10. **Hit, then incremental edit.** In B, edit a function body in `mylib`,
    build, run the binary: new behavior. Edit again: assert the second compile
    reused the session (the session folder's `s-*` name changed, the lock-file
    prefix count stays 1) and the binary is right.
11. **Edit, revert, edit (F2).** In A: edit → build (session at v1); revert →
    build (hit for v0); assert `stat` link count/inode of the session's
    `metadata.rmeta` is unchanged and its bytes are still v1; touch the v1
    source back and build with the cache entry for v1 removed (or
    `SCCACHE_RECACHE=1`); assert the produced `.rmeta` equals a from-scratch
    v1 build in a third checkout and the binary prints v1 values.
12. **Dependent after a cache-served dependency.** Workspace `la` ← `lb` ←
    bin. In B (all hits), edit `lb` only: `lb` misses and compiles
    incrementally against the cache-served `la`; binary correct. Then make A
    produce a new `la` (edit, build), `git`-style copy the same source to B and
    build: `la` is a hit, `lb` recompiles with its session against the new
    `la`; compare `lb`'s `.rmeta` with a from-scratch build and run the binary.
13. **Session from another commit.** In B, build commit 1 with edits to `la`
    (session), switch the sources to commit 2 where A already built
    everything: all hits, no rustc run for `la` (stats), binary is commit 2's.
    Then edit `la` on top of commit 2 and build: correct output from the stale
    session.
14. **Modes do not mix.** Build A with incremental, B with
    `CARGO_INCREMENTAL=0`: third-party crates hit, workspace crates miss in B
    (documents F5); then A again: still hits its own entries. Add a direct
    `rustc` pair (same arguments, one with `-C incremental`) in
    `tests/sccache_rustc.rs` to show the marker separates them (R3).
15. **Store policy (R5).** After test 10's second edit, a third checkout at
    that exact source misses for `mylib` (nothing stored from the reused
    session) and hits after it has built once cold.
16. **Killed compile.** Start a build of a large generated crate, kill the
    client's rustc, rebuild: success, and the stats show no store for the
    killed compile.
17. **Stats and stderr.** A hit replays the warnings of the incremental
    producer; `"incremental"` no longer appears under not-cached reasons.
    Replace `test_rust_cargo_incremental_refused_by_default` and
    `test_rust_cargo_incremental_allowed_deps_cached`
    (`tests/sccache_cargo.rs:665`, `:691`).
18. **`cargo check`** variant of 9 and 10 (`--emit=dep-info,metadata`).
19. **Pilot gate:** run tests 10-13 with `RUSTFLAGS=-Zincremental-verify-ich`
    on the pinned nightly once, as a fingerprint check of the scenarios
    Layer 3 makes common (stale session + swapped dependency).

Measurement for P2, in addition to the plan's table: edit loop with the
feature off, on, and on with R9; number and bytes of cache writes per edit
round; count of `s-*-working` folders after a build (must be 0 for hit-only
crates).

## Open questions

1. R5/R9 detection: compute the session folder in sccache, or use a marker
   file? Needs a decision and a check that the `StableCrateId` hash can be
   reproduced for 1.95-1.99 (not attempted here).
2. How large F7 is for the real projects. Exp 9 is one crate; the share of
   the preflight in a full edit round (which also links tests) is unknown
   until P2.
3. macOS and Windows were not tested (hard-link behavior, `unpacked` debuginfo
   keeps objects, not `.dwo`). Needed for upstream, not for Leto.
4. `staticlib` with LTO and incremental, and `-Zno-embed-metadata` /
   stub-metadata rlibs (`rustc:rustc_metadata/src/fs.rs:49-56`), were not
   tested.
5. Whether a rustc release in the 1.95-1.99 range has a known
   incremental-soundness issue was not researched (no issue-tracker search);
   R5 is the structural answer either way.
6. Upstream's own reasoning in mozilla/sccache#236 is thin: the 2018 idea was
   to store the session folder in the cache, and maintainers later only said
   incremental is not on their roadmap. Nothing there argues against this
   design, and nothing supports it either.

## Appendix: experiments

Scratch sources and outputs:
`/home/leovigna/.cache/t3-agent-tmp/claude-1000/-home-leovigna-workspaces-leto-labs-sccache/651315c4-071d-483e-9d35-0c30cafe0e16/scratchpad/`
(`exp/`, `ws/`, `ws2/`; session-specific, may be cleaned). Test crates: `a`
(two modules, a generic, an `#[inline]` fn, a trait impl, a const), `b`
(uses all of them), `c` (bin).

| Exp | What | Result |
|---|---|---|
| 1 | `a` built 9 ways (O0/O1/O3 × incremental on/off, codegen-units 1/16/256, two incremental folders) | `.rmeta` differs on/off at every level; equal across codegen-units and across folders; rlib members 3 vs 26-28; incremental objects equal between runs except the member-name suffix |
| 2 | Preflight commands with `-C incremental` | `--print file-names`: nothing created. `--emit dep-info`: creates `s-…-working` each run, 2 leftover files, never finalized; dep-info text equal to the run without the flag |
| 3 | `stat` of `deps/liba.rmeta` and session `metadata.rmeta` | Same inode, `nlink=2`, on 1.95.0, 1.97.1, 1.99.0, 1.99.1, nightly |
| 4 | v1 session, then v0 outputs placed by rename vs written in place, then rebuild v1 | rename: correct v1. In place: session file became v0 and rustc emitted v0 metadata for v1 source |
| 5 | One session for `b`; `a` swapped through 6 variants | `b`'s `.rmeta` equal to from-scratch each time; binary output right. 5b: mismatched pair gives E0460 |
| 6 | `kill -9` of an incremental compile, then rebuild | Only `-working` left, no outputs; rebuild fine; edit rebuild 0.48 s vs full 2.17 s |
| 7 | cargo 1.99.1 `build -v` / `test --no-run -v` on a 3-crate workspace, incremental on vs `CARGO_INCREMENTAL=0` | Different `-C metadata` and `-C extra-filename` for workspace crates; no `-C codegen-units`; libs get `.rmeta` externs; tests get `--test`, `.rlib` externs, own session folders with the same crate-name prefix |
| 8 | Output folder listing per version for default, debuginfo, split-debuginfo unpacked/packed, save-temps, staticlib | Same 3 files in both modes except `unpacked` (1 vs 26 `.dwo`) and `save-temps`; no incremental path in any output |
| 9 | syn 2.0.119 as a workspace crate, dev profile | Edit rebuild 0.66 s, unchanged rebuild 0.58-0.61 s, dep-info preflight 0.17-0.19 s, cold incremental 2.59 s, non-incremental 1.98 s, rlib 29 MB |

Not done: no sccache server was started and no sccache test was run, as
instructed. Every statement about sccache's behavior is from reading the
source; the end-to-end tests above are what would confirm them.
