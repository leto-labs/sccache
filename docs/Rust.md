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


## Incremental compilation

Cargo enables incremental compilation for workspace members and path
dependencies in the dev profile. The server's `rust_incremental` setting
controls how sccache handles invocations with `-C incremental=DIR`.
`SCCACHE_RUST_INCREMENTAL` overrides the config file. Restart the server to
change the mode. `SCCACHE_RUST_INCREMENTAL` replaces the former
`SCCACHE_ALLOW_INCREMENTAL` setting. `CARGO_INCREMENTAL=1` can be used in
any mode.

- `off` (default) runs incremental invocations uncached.
- `cache` looks up eligible invocations and stores outputs from confirmed
  cold local compiles. Once sccache knows that a unit has a finalized local
  session, later invocations skip the cache and use that session directly.
- `cache-no-bypass` always looks up eligible invocations and applies the same
  cold-session rule to writes.

Set `SCCACHE_RUST_INCREMENTAL=cache` in the build environment as well as on
the server to bypass cache traffic before connecting. Keep the client's
basedirs settings aligned with the server's when using path remapping.
Warm compiles that bypass in the client do not appear in server statistics.

In either cache mode, sccache runs eligible units with a private session
directory named `DIR/<crate_name>-sccache-<unit hash>`. Switching from `off`
to a cache mode starts each unit cold the first time it compiles in its private
directory. Switching back selects Cargo's original session location. Both
cache modes share the private directory. sccache refreshes its modification
time after each local compile, including bypasses, so mtime-based cleanup can
recognize active sessions. `cargo clean -p` removes a package's private
directories along with its other sessions. Cache hits restore the normal rustc
outputs without creating or modifying session directories.
Upgrading from `leto-0.19.0-3` or `leto-0.19.0-4` changes the private names.
The old sessions are left for cleanup, and each unit starts cold the first
time it compiles in the new directory.

Incremental and non-incremental builds use separate entries, regardless
of the name of the incremental directory. Cargo forwards variables such as
`CARGO_INCREMENTAL` and `CARGO_PROFILE_DEV_INCREMENTAL` to registry dependency
compiles, where their values participate in non-incremental cache keys. Use
consistent Cargo profile settings to share those entries across builds.

Incremental builds run locally. Invocations with `-C save-temps` enabled or
with unpacked split debuginfo and debuginfo enabled run uncached. This includes
Cargo dev profiles on macOS that use unpacked split debuginfo. Combined short
flag groups such as `-gCincremental=DIR` also run uncached.

In `--show-stats`, incremental lookups count under Rust hits and misses.
`Incremental: not stored, session reused` counts successful misses that
started with a finalized session or whose session directory could not be read.
Under `Non-cacheable reasons`, `incremental session` counts bypasses of known
local sessions.
