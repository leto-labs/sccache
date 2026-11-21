sccache includes support for caching Rust compilation. This includes many caveats, and is primarily focused on caching rustc invocations as produced by cargo. A (possibly-incomplete) list follows:
* `--emit` is required.
* `--crate-name` is required.
* Only `link`, `metadata` and `dep-info` are supported as `--emit` values. An explicit emit list must include `link` or `metadata`.
* `--out-dir` is required.
* `-o file` is not supported.
* Compilation from stdin is not supported, a source file must be provided.
* Values from `env!` require Rust >= 1.46 to be tracked in caching.
* With `SCCACHE_BASEDIRS`, the values of variables read with `env!` or `option_env!` are hashed with the basedirs stripped by default, so a crate that bakes `env!("CARGO_MANIFEST_DIR")` into its outputs can get a cache hit from another checkout and use that checkout's path. `SCCACHE_BASEDIRS_VERBATIM_ENV_DEPS=CARGO_MANIFEST_DIR,CARGO_MANIFEST_PATH` hashes those values verbatim, so only the crates that read them are cached per checkout. Linked outputs always hash these values verbatim. A procedural macro that reads the variable with `std::env::var` is not covered, as rustc does not list it in the dep-info. Listing `OUT_DIR` would stop sharing every crate that `include!`s generated code.
* With `SCCACHE_BASEDIRS`, a hit can restore a crate compiled in another checkout, with that checkout's paths in its metadata and debug info. With `SCCACHE_BASEDIRS_REMAP_PATHS=1` in the environment of the compile, sccache passes rustc `--remap-path-prefix` so that these outputs are the same in every checkout. Paths under a basedir that holds the working directory become relative to the working directory, so diagnostics still lead to the files, but the compilation directory in the debug info is `.` and a debugger has to be started in the directory cargo ran rustc in. Other paths under a basedir that holds the output directory, such as the `OUT_DIR` of a crates.io dependency, are written under `/sccache/basedir`, which names no local file. A `--remap-path-prefix` of the build's own takes precedence where both match. Such compiles are not distributed.
* Procedural macros that read files from the filesystem may not be cached properly.
* With linked caching off, sccache identifies procedural macros by their crate metadata. Edits to native code linked into a proc-macro can leave that metadata unchanged.

If you are using Rust 1.18 or later, you can ask cargo to wrap all compilation with sccache by setting `RUSTC_WRAPPER=sccache` in your build environment.

To compute the cache key of a crate, sccache runs `rustc --emit=dep-info` to list its source files, and `rustc --print file-names` to list its outputs. The server remembers what both found, and reuses it for a later compile of the crate with the same compiler, arguments, working directory, environment, externs, static libraries and target spec, as long as every listed source file still has the same contents. This lasts for the life of the server; nothing is written to the cache. As with the preprocessor cache, a file that was not listed does not invalidate it: for instance, adding `foo/mod.rs` next to an existing `foo.rs` keeps reusing the cached result instead of failing with an ambiguous module error.

## Cargo environment variables

The fork defaults to `rust_hash_cargo_env = "semantic"`. It excludes known
Cargo settings from the blanket `CARGO_*` hash, including target directories,
network and terminal settings, and profile and rustflags settings that reach
rustc through its arguments. Package metadata, Cargo variables used by
compilers, and unknown `CARGO_*` variables still participate in the key. Values
that rustc lists in dep-info, including reads through `env!` and `option_env!`,
are always hashed under the existing path normalization rules.

Set `rust_hash_cargo_env = "all"` at the top level of the server config to
use upstream's blanket hash. Both modes exclude jobserver data,
`CARGO_REGISTRIES_*`, `CARGO_TARGET_DIR`, and `CARGO_ENCODED_RUSTFLAGS`.
The `all` mode excludes every `CARGO_BUILD_*` variable. The `semantic` mode
excludes the same known Cargo settings as Leto 0.19.0-12 and still hashes
unknown `CARGO_BUILD_*` variables for key compatibility. Incremental
compiles in either mode exclude Cargo's incremental settings. The linked
output key also includes its linker environment inputs.

`SCCACHE_RUST_HASH_CARGO_ENV=all` or `semantic` overrides the config file.
Restart the server to change the mode.

## Caching test, binary and proc-macro targets

Set `rust_cache_linked = "check"` at the top level of the server config or
set `SCCACHE_RUST_CACHE_LINKED=check` when starting the server. The environment
variable takes precedence. Restart the server to change the mode. The default
is `off`.

The stages are cumulative.

| Mode | Target coverage |
| --- | --- |
| `off` | Default Rust caching rules |
| `check` | Also caches tests, binaries, examples, benches and proc-macros when `--emit` includes `metadata` and omits `link` |
| `proc-macro` | Also caches eligible linked proc-macros |
| `tests` | Also caches eligible linked unit and integration test harnesses, including custom integration tests with `harness = false` |
| `all` | Also caches eligible linked binaries, examples, build scripts and custom benches without `--cfg test` |

The `check` stage also covers metadata-only invocations that omit
`--crate-type`. It derives `lib<crate><extra-filename>.rmeta` directly,
including empty metadata files for tests and binaries. These lookups appear as
Rust hits and misses in `--show-stats`.

### Linked outputs

Linked caching supports native Linux GNU targets with a GNU-style linker
that supports `--dependency-file`. It covers `--crate-type proc-macro`,
Cargo's `--test` invocations without an explicit crate type, and
`--crate-type bin`, according to the selected stage. For linked invocations
that omit both `--crate-type` and `--test`, `--cfg test` selects the `tests`
stage. Other implicit binaries require `all`. The executable is
`<out-dir>/<crate><extra-filename>`. Linked hits and misses count under Rust
in `--show-stats`. Implicit binaries excluded by the selected stage count
under the `crate-type` non-cacheable reason. `cdylib` and `dylib` compile uncached.

sccache restores the output and its mode through a temporary-file rename,
including with the [disk cache's reflink support](Local.md#reflink-cache-hits).
The rename leaves any running executable or existing uplifted hard link on its
old inode. Cargo can then uplift the new executable or run a restored build
script to regenerate its outputs. sccache regenerates dep-info for each
checkout. Dependent Rust crates hash the whole ELF proc-macro dylib, including
any native code linked into it.

The lookup key includes the usual Rust compilation inputs, the linker driver
and linker executable digests and version output, the target and linker
arguments including ordered library search paths, and these environment
variables.

- `LIBRARY_PATH`, `LD_LIBRARY_PATH`, `LD_RUN_PATH`, `GCC_EXEC_PREFIX`, and
  `COMPILER_PATH`
- `SOURCE_DATE_EPOCH`, `SDKROOT`, `MACOSX_DEPLOYMENT_TARGET`,
  `CCC_OVERRIDE_OPTIONS`, `LDEMULATION`, and variables prefixed with
  `MOLD_` or `LLD_`

The linker environment is hashed verbatim with one exception. Cargo puts
checkout-specific directories in `LD_LIBRARY_PATH`, so entries under a basedir
are mapped to their relative paths and the manifest verifies the libraries
in those directories.

Variables read through `env!` or `option_env!` are also hashed verbatim for
linked outputs. A harness that embeds `CARGO_MANIFEST_DIR`,
`CARGO_BIN_EXE_*` or `CARGO_TARGET_TMPDIR` will miss when that path changes.
Files read with `include_bytes!` or `include_str!` are hashed from dep-info,
including files outside the crate.

On a miss rustc asks the linker to write its dependency file. The cache
entry includes a manifest of the linker-reported input paths, sizes,
modification times, and content digests. It omits objects and other temporary
files generated by rustc for that invocation. Paths under the sysroot or a
configured basedir are resolved against the current build's corresponding
directory. A hit requires every recorded file to exist and have the same
content digest. sccache also checks relevant files in the library search
directories so a newly shadowing library can invalidate the entry. Use
trusted linkers, compiler drivers, toolchains, and cache storage. The check
depends on the linker's report and the same input-tracking rules for
procedural macros and compiler plugins as ordinary Rust caching.

sccache skips storing a compile whose linker produces no usable dependency
file. It runs unsupported linker flavors, multiple crate types, enabled
rpath, linker-plugin LTO, explicit LTO, coverage, profile generation or use,
sanitizers, link-dead-code, save-temps, thin archives, and split debuginfo other than `off`
uncached. A finalized incremental session bypasses the cache. Linked
compiles run locally. Nonempty `LD_PRELOAD`, `LD_AUDIT`, and
`CCC_OVERRIDE_OPTIONS` also disable linked caching.

The server skips storing linked compiles whose uncompressed compiler outputs
exceed `rust_cache_linked_max_entry_bytes` (256 MiB by default). Set
`SCCACHE_RUST_CACHE_LINKED_MAX_ENTRY` to override it in bytes. Both settings
are read at server startup. Oversize compiles count under
**Rust linked: not stored, entry too large** in `--show-stats`
(`rust_linked_entry_too_large` in JSON). Linked entries share the disk cache's
normal size limit and reflink accounting.

### Comparing restored and rebuilt test harnesses

Start the server under test with the `tests` or `all` stage, then warm its
cache with `cargo test --workspace --no-run` in one checkout. Set
`SCCACHE_BASEDIRS` to that checkout's root and use the same toolchain, profile,
and Rust flags in the second checkout.

```sh
SCCACHE_BASEDIRS_REMAP_PATHS=1 leto/linked-soak.sh /work/checkout-b --run
```

The script needs Python 3.9 or later and `objcopy`. Use an empty Cargo target
directory in the second checkout. A custom Cargo build directory must be
inside that target directory. The script uses the
configured sccache server and `RUSTC_WRAPPER` (default `sccache`) and defaults
`SCCACHE_BASEDIRS` to the checkout root. It builds the workspace test harnesses
with the cache, optionally runs them with `--run`, then builds the same units
from scratch at the same target path with `SCCACHE_NO_CACHE=1`. The rebuilt
outputs are left in the target directory.

The report compares raw bytes and, for mismatches, compares copies with
debug sections and the GNU build ID removed by `objcopy`. Set
`SCCACHE_BASEDIRS_REMAP_PATHS=1` for both checkouts to expect byte equality.
Without remapping, debug paths can differ. The script exits nonzero if a
harness is missing, normalized bytes differ, raw bytes differ with remapping
enabled, or a restored test fails.

### Soaking stage `all`

On a shared host, run the two-checkout driver in a private network namespace.
Replace the paths with the project repository, a fresh work directory, and
the sccache binary under test.

```sh
leto/test-sandboxed.sh leto/linked-soak-host.sh /work/project /work/soak-all 4241 \
  "$PWD/target/release/sccache" --all --remap
```

The driver starts a private server in stage `all`. It runs
`cargo build --workspace --bins --examples --message-format=json` and
`cargo test --workspace --no-run --message-format=json` to fill the cache.
The soak then runs both commands in the second checkout, once with the cache
and once with `SCCACHE_NO_CACHE=1`. With an already warmed cache, point the
comparison script directly at the second checkout from inside the namespace.

```sh
SCCACHE_BASEDIRS_REMAP_PATHS=1 leto/linked-soak.sh /work/checkout-b \
  --targets all --warm-checkout /work/checkout-a
```

The report includes raw and stripped comparisons for bins, examples, test
harnesses, and build-script executables. For each build-script execution it
compares the `OUT_DIR` tree file by file and Cargo's `output` file, which
contains the build script's stdout and `cargo:` directives. UTF-8 text comparisons
replace the absolute checkout and Cargo target paths with placeholders. The
driver supplies both checkouts for this normalization. The soak fails on
missing outputs or normalized differences, and `--remap` also requires raw
byte equality.

Use `--run` to run the restored test harnesses. Repeat
`--run-bins 'my-bin "argument with spaces"'` to smoke-run named restored
binaries with their arguments.

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
the server to bypass cache traffic before connecting. For targets enabled by
`rust_cache_linked`, also set the same linked cache mode in the build environment.
Keep the client's basedirs settings aligned with the server's when using path
remapping.
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

The client also uses its incremental mode and first-build settings when it
runs rustc for an uncached compile, including after losing the server
connection. In either cache mode with `plain`, a binary, example, test,
proc-macro, or metadata-only build starts plain if its expected rustc output
is missing. The first rebuild with an existing output creates a session in
Cargo's original incremental directory. The warm-session client bypass still
uses its private directory.

Rustc leaves the incremental directory path untracked, so its value has no
effect on the output. A plain compile produces a complete artifact.
Binaries and tests have no dependent crates. For proc-macros, rustc
validates the dependency's stable version hash (SVH) when reusing a
dependent's session, as it does for libraries. The plain command uses
`-C lto=off` to match incremental codegen unless the invocation sets LTO.

Cache-eligible incremental compiles in either cache mode use a private
session directory named `DIR/<crate_name>-sccache-<unit hash>`. Switching from `off` to a cache
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
With `rust_hash_cargo_env = "all"`, Cargo variables such as
`CARGO_INCREMENTAL` and `CARGO_PROFILE_DEV_INCREMENTAL` participate in
non-incremental cache keys.

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
a specific argument such as `arg:-C opt-level`. `dependency` names the changed
externs. `unchanged` means the same key was previously stored and is now missing.
For a changed key, the server compares against the stored manifest with the fewest
differences. Source and library paths use the same basedir stripping as the
key. Argument components are grouped by flag and include the option name for
`-C` and `-Z` and the full value for `--cfg`. All other argument values and all
environment values are recorded as digests. Comparisons against older
manifests can show the old `arguments` component being replaced by individual
argument components. The reader follows changed extern artifact names to
earlier misses in the same checkout and reports unresolved externs when a
producer is absent from the log. Pass `--json` to include the root crates in
JSON output.

The server stores up to four distinct keys per crate identity under
`$SCCACHE_DIR/explain` as it writes cache entries. This history survives a
server restart, and existing cache hits do not populate it. Keep the log
outside the cache entry directories. The log appends across builds and restarts.
This mode routes client-side builds through the server for diagnostics.
Warm incremental compiles that bypass the cache produce no miss record.
Remove the setting and any environment override, then restart the server to
turn the mode off.
