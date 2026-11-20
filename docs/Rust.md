sccache includes support for caching Rust compilation. This includes many caveats, and is primarily focused on caching rustc invocations as produced by cargo. A (possibly-incomplete) list follows:
* `--emit` is required.
* `--crate-name` is required.
* Only `link`, `metadata` and `dep-info` are supported as `--emit` values. An explicit emit list must include `link` or `metadata`.
* `--out-dir` is required.
* `-o file` is not supported.
* Compilation from stdin is not supported, a source file must be provided.
* Values from `env!` require Rust >= 1.46 to be tracked in caching.
* With `SCCACHE_BASEDIRS`, the values of variables read with `env!` or `option_env!` are hashed with the basedirs stripped, so a crate that bakes `env!("CARGO_MANIFEST_DIR")` into its outputs can get a cache hit from another checkout and use that checkout's path. `SCCACHE_BASEDIRS_VERBATIM_ENV_DEPS=CARGO_MANIFEST_DIR,CARGO_MANIFEST_PATH` hashes those values verbatim, so only the crates that read them are cached per checkout. A procedural macro that reads the variable with `std::env::var` is not covered, as rustc does not list it in the dep-info. Listing `OUT_DIR` would stop sharing every crate that `include!`s generated code.
* With `SCCACHE_BASEDIRS`, a hit can restore a crate compiled in another checkout, with that checkout's paths in its metadata and debug info. With `SCCACHE_BASEDIRS_REMAP_PATHS=1` in the environment of the compile, sccache passes rustc `--remap-path-prefix` so that these outputs are the same in every checkout. Paths under a basedir that holds the working directory become relative to the working directory, so diagnostics still lead to the files, but the compilation directory in the debug info is `.` and a debugger has to be started in the directory cargo ran rustc in. Other paths under a basedir that holds the output directory, such as the `OUT_DIR` of a crates.io dependency, are written under `/sccache/basedir`, which names no local file. A `--remap-path-prefix` of the build's own takes precedence where both match. Such compiles are not distributed.
* Procedural macros that read files from the filesystem may not be cached properly.
* A procedural macro is identified by its crate metadata, so a change to native code it links, with no change to its Rust source or dependencies, is not seen.
* Crates that invoke the system linker cannot be cached. Examples are `bin`, `dylib`, `cdylib`, and `proc-macro` crates.

If you are using Rust 1.18 or later, you can ask cargo to wrap all compilation with sccache by setting `RUSTC_WRAPPER=sccache` in your build environment.

To compute the cache key of a crate, sccache runs `rustc --emit=dep-info` to list its source files, and `rustc --print file-names` to list its outputs. The server remembers what both found, and reuses it for a later compile of the crate with the same compiler, arguments, working directory, environment, externs, static libraries and target spec, as long as every listed source file still has the same contents. This lasts for the life of the server; nothing is written to the cache. As with the preprocessor cache, a file that was not listed does not invalidate it: for instance, adding `foo/mod.rs` next to an existing `foo.rs` keeps reusing the cached result instead of failing with an ambiguous module error.

## Metadata-only target caching

Set `rust_cache_linked = "check"` at the top level of the server config or
set `SCCACHE_RUST_CACHE_LINKED=check` when starting the server. The environment
variable takes precedence. Restart the server to change the mode. The default
is `off`.

The stages are cumulative.

| Mode | Target coverage |
| --- | --- |
| `off` | Default Rust caching rules |
| `check` | Also caches tests, binaries, examples, benches and proc-macros when `--emit` includes `metadata` and omits `link` |
| `proc-macro` | Reserved for linked proc-macros |
| `tests` | Reserved for linked test harnesses |
| `all` | Reserved for linked binaries, examples and build scripts |

The reserved stages warn at startup and use `check`. The `check` stage derives
the `.rmeta` filename directly, including empty metadata files for tests and
binaries. These lookups appear as Rust hits and misses in `--show-stats`.

## Incremental compilation

Cargo enables incremental compilation for workspace members and path
dependencies in the dev profile. The server's `rust_incremental` setting
controls how sccache handles invocations with `-C incremental=DIR`.
`SCCACHE_RUST_INCREMENTAL` overrides the config file. Restart the server to
change the mode. `SCCACHE_ALLOW_INCREMENTAL=cache` in the compile's
environment selects `cache` when the configured mode is `off`.
`CARGO_INCREMENTAL=1` can be used in any mode.
`SCCACHE_ALLOW_INCREMENTAL=1` leaves the configured mode in effect.

- `off` (default) runs incremental invocations uncached.
- `cache` looks up eligible invocations and stores outputs from confirmed
  cold local compiles. Once sccache knows that a unit has a finalized local
  session, later invocations skip the cache and use that session directly.
- `cache-no-bypass` always looks up eligible invocations and applies the same
  cold-session rule to writes.

Set `SCCACHE_RUST_INCREMENTAL=cache` in the build environment as well as on
the server to bypass cache traffic before connecting. For metadata-only
targets enabled by `rust_cache_linked`, also set the same linked cache mode
in the build environment. Keep the client's basedirs settings aligned with
the server's when using path remapping.
Warm compiles that bypass in the client do not appear in server statistics.

In either cache mode, a cache miss for a Rust target with a known metadata
output path, no existing `.rmeta` and no finalized session compiles without
`-C incremental` by default. This avoids cold incremental overhead and defers
session creation until the first rebuild. Cargo keeps the previous `.rmeta`,
including metadata-only builds from `cargo check`. A target with existing
metadata, or whose metadata path cannot be determined, compiles incrementally.
Such a plain compile also gets `-C lto=off` unless the command sets `-C lto`:
incremental compiles skip rustc's thin local LTO across codegen units, and
above `opt-level=0` that extra work would cost more than the session it saves.
Set `rust_incremental_first_build = "incremental"` in the server config or
`SCCACHE_RUST_INCREMENTAL_FIRST_BUILD=incremental` when starting the server
to use incremental codegen on the first miss as well. The default is `plain`.

Incremental compiles in either cache mode use a private session directory
named `DIR/<crate_name>-sccache-<unit hash>`. Switching from `off` to a cache
mode starts each unit cold the first time it compiles in its private directory.
Switching back selects Cargo's original session location. Both cache modes
share the private directory. sccache refreshes its modification time after
each local compile that uses it, including bypasses, so mtime-based cleanup
can recognize active sessions. `cargo clean -p` removes a package's private
directories along with its other sessions. Cache hits restore the normal
rustc outputs without creating or modifying session directories.
Upgrading from `leto-0.19.0-3` or `leto-0.19.0-4` changes the private names.
Each unit starts cold the first time it compiles in the new directory.

After a compile in a private session, sccache can prune older private and
flat session directories for the same crate in that incremental directory.
The sweep runs at most once per hour per crate and incremental directory.
It keeps the current session and directories with a lock file modified
within the past hour. `incremental_prune_days` (overridden by
`SCCACHE_INCREMENTAL_PRUNE_DAYS`) sets the minimum age. The default is 7 days,
and `0` disables pruning. These rules also apply when the client bypasses
the daemon and runs rustc directly.

Invocations with and without `-C incremental` use separate entries,
regardless of the incremental directory name. An incremental invocation uses
the same key whether its cold compile runs plain or incremental. Both produce
the requested outputs, and cache hits reuse whichever output was stored.
Dependents hash the exact metadata they compile against, and rustc validates
upstream crates by SVH when reusing a dependent's incremental session.
Cargo forwards variables such as `CARGO_INCREMENTAL` and
`CARGO_PROFILE_DEV_INCREMENTAL` to registry dependency compiles, where their
values participate in non-incremental cache keys. Use consistent Cargo
profile settings to share those entries across builds.

Incremental builds run locally. Invocations with `-C save-temps` enabled or
with unpacked split debuginfo and debuginfo enabled run uncached. This includes
Cargo dev profiles on macOS that use unpacked split debuginfo. Combined short
flag groups such as `-gCincremental=DIR` also run uncached.

In `--show-stats`, incremental lookups count under Rust hits and misses.
`Incremental: not stored, session reused` counts successful misses that
started with a finalized session or whose session directory could not be read.
Under `Non-cacheable reasons`, `incremental session` counts bypasses of known
local sessions.

## Explain cross-worktree cache misses

Set `miss_explain` in the [configuration file](Configuration.md) or set
`SCCACHE_MISS_EXPLAIN` to an absolute log path when starting the server.
For a fresh baseline, use a dedicated server and disk cache. Build a known-good checkout
first, then build the checkout you want to compare using the same Cargo
command and profile. Each build should set `SCCACHE_BASEDIRS` to its own root.

```sh
export SCCACHE_SERVER_PORT=4241
export SCCACHE_DIR="$HOME/.cache/sccache-explain"
export SCCACHE_MISS_EXPLAIN="$HOME/sccache-misses.jsonl"
sccache --start-server
(cd /work/checkout-a && SCCACHE_BASEDIRS="$PWD" RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 cargo build)
(cd /work/checkout-b && SCCACHE_BASEDIRS="$PWD" RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 cargo build)
python3 leto/explain-misses.py "$SCCACHE_MISS_EXPLAIN"
sccache --show-stats
sccache --stop-server
```

The server appends a JSON line for each Rust cache lookup that returns a
miss. `new` means there is no history for the crate's compilation identity.
`own-input` lists inputs such as a source file, an environment variable, or
the argument digest. `dependency` names the changed externs. `unchanged`
means the same key was previously stored and is now missing. For a changed
key, the server compares against the stored manifest with the fewest
differences. Source and library paths use the same basedir stripping as the
key. Arguments and environment values appear only as digests. The reader
follows changed extern artifact names to earlier misses in the same
checkout and reports unresolved externs when a producer is absent from the
log. Pass `--json` to include the root crates in JSON output.

The server stores up to four distinct keys per crate identity under
`$SCCACHE_DIR/explain` as it writes cache entries. This history survives a
server restart, and existing cache hits do not populate it. Keep the log
outside the cache entry directories. The log appends across builds and restarts.
This mode routes client-side builds through the server for diagnostics.
Warm incremental compiles that bypass the cache produce no miss record.
Remove the setting and any environment override, then restart the server to
turn the mode off.
