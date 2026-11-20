# Local

sccache defaults to using local disk storage. You can set the `SCCACHE_DIR` environment variable to change the disk cache location. By default it will use a sensible location for the current platform: `~/.cache/sccache` on Linux, `%LOCALAPPDATA%\Mozilla\sccache` on Windows, and `~/Library/Caches/Mozilla.sccache` on MacOS.

The default cache size is 10 gigabytes. To change this, set `SCCACHE_CACHE_SIZE`, for example `SCCACHE_CACHE_SIZE="1G"`.

The local storage only supports a single sccache server at a time. Multiple concurrent servers will race and cause spurious build failures.

## Restarting a shared server safely

On Unix, `SIGTERM`, `SIGINT`, and `sccache --stop-server` close the listener
and give existing connections and cache writes time to finish. A `SIGTERM`
received during the drain ends the wait immediately.

For a shared server, put the following settings in the [sccache configuration](Configuration.md)
that the server and clients read. `SCCACHE_SERVER_AUTOSTART=0` also disables
autostart for clients on a managed host.

```toml
[server]
shutdown_drain_secs = 300
autostart = false
connect_wait_secs = 30
```

Run the server in the foreground under systemd. Use `KillMode=mixed` so
systemd sends the initial signal only to the server and lets compiler
children finish during the drain. Set `TimeoutStopSec` to at least the drain
time plus 30 seconds.

```ini
[Service]
Environment=SCCACHE_START_SERVER=1 SCCACHE_NO_DAEMON=1
ExecStart=%h/.cargo/bin/sccache
KillSignal=SIGTERM
KillMode=mixed
TimeoutStopSec=330s
```

Swap the sccache binary before adding these keys to the shared configuration.
Clients are short-lived, usually one process per compile. Give existing
clients a few minutes to finish, then restart the managed server. The running
daemon must support signal handling for that restart to drain. New clients
can compile locally if they lose their connection during the first upgrade.

## Reflink cache hits

Set `reflink = true` under `[cache.disk]` or set `SCCACHE_DISK_REFLINK=true` before starting the server to enable reflinks for a standalone Linux disk cache. The default is `false`. The server probes the cache directory at startup and uses the usual compressed entries if the probe fails.

With reflinks enabled, new entries store objects as aligned, uncompressed ZIP members. A hit can share the aligned part of the object with the cache when both files are on a filesystem that supports reflinks, such as btrfs. Any remaining bytes are copied. If cloning fails, sccache copies the object. Existing compressed entries remain usable. Client-side compilation uses the daemon's raw entry setting and writes compressed entries when connected to an older daemon.

When the daemon compiles an object on a miss, it also tries to clone the aligned part of each output into the new cache entry. If cloning fails, it packs and copies the raw entry. With client-side compilation, the client packs the entry and sends its bytes to the daemon.

The disk cache limit uses each entry's expected on-disk size. On btrfs with 4 KiB sectors and a `zstd` compression property such as `zstd:1` on the cache directory, sccache estimates the size of raw entries in the background using zstd level 1 on 128 KiB chunks. It saves the estimate in the entry's `user.sccache.weight` extended attribute and reads it when loading the cache index. Other filesystems, existing entries without an estimate, and entries whose extended attributes cannot be read or written count at file length. Set the compression property before starting sccache. The estimate can differ from actual btrfs allocation.

Eviction removes the cache's reference to shared extents. Reflinked worktree outputs remain intact through copy-on-write, and the filesystem frees those extents only when their last reference is gone.

Older sccache versions treat hits on uncompressed entries as misses. Remove `reflink` from the TOML configuration before starting an older version.

## Preprocessor cache mode

This is inspired by [ccache's direct mode](https://ccache.dev/manual/3.7.9.html#_the_direct_mode) and works roughly the same.
It adds a cache that allows to skip preprocessing when compiling C/C++. This can make it much faster to return compilation results
from cache since preprocessing is a major expense for these.

Preprocessor cache mode is controlled by a configuration option which is true by default, as well as additional conditions described below.

To ensure that the cached preprocessor results for a source file correspond to the un-preprocessed inputs, sccache needs
to remember, among other things, all files included by the source file. sccache also needs to recognize
when "external factors" may change the results, such as system time if the `__TIME__` macro is used
in a source file. How conservative sccache is about some of these external factors is configurable, see below.

Preprocessor cache mode will be disabled in any of the following cases:

- Not compiling C or C++
- The configuration option is false
- Not using GCC or Clang
- Not using local storage for the cache
- Any of the compiler options `-MP`, `-Xpreprocessor`, `-Wp,` are present
- The modification time of one of the header files is too new (avoids a race condition)
- Certain strings such as `__DATE__`, `__TIME__`, `__TIMESTAMP__` are present in the source code,
  indicating that the preprocessor result may change based on external factors

The preprocessor cache may silently produce stale results in any of the following cases:

- When a source file was compiled and its results were cached, a header file would have been included if it existed, but it did
  not exist at the time. sccache does not know about such files, so it cannot invalidate the result if the header file later exists.
  With `basedirs` this widens: a header that exists in one checkout and not in another, at a position the compiler searches first
  (the including file's own directory, for a quoted include, or an earlier `-I`), makes the two checkouts resolve the same
  `#include` to different files. sccache only remembers the files the preprocessor opened, never the directories it looked in and
  found nothing, so it cannot tell the two apart and the second checkout can be handed the first one's object. Compiler search-path
  precedence is not modelled.
- A macro such as `__TIME__` (etc) is used in the source code and `ignore_time_macros` is enabled
- There are other external factors influencing the preprocessing result that sccache does not know about

On Linux and macOS, the server remembers the digest of each file it hashes for this mode and reuses it while the file's size,
mtime, ctime, device and inode are unchanged, so a cache hit does not re-read unchanged headers. Files changed less than two
seconds before being hashed are not remembered, as their timestamps may not tell a quick rewrite apart.

When [`basedirs`](Configuration.md) is set, headers under a base directory are cached under the path suffix excluding
the base, so that multiple checkouts that differ only by the base share a cache entry. A single translation unit uses
at most one base directory: the one containing the input file, or the working directory. Headers under other basedirs
are not shared.

Configuration options and their default values:

- `use_preprocessor_cache_mode`: `true`. Whether to use preprocessor cache mode. This can be overridden for an sccache invocation by setting the environment variable `SCCACHE_DIRECT` to `true`/`on`/`1` or `false`/`off`/`0`.
- `file_stat_matches`: `false`. If false, only compare header files by hashing their contents. If true, will use size + ctime + mtime to check whether a file has changed. See other flags below for more control over this behavior.
- `use_ctime_for_stat`: `true`. If true, uses the ctime (file status change on UNIX, creation time on Windows) to check that a file has/hasn't changed. Can be useful to disable when backdating modification times in a controlled manner.

- `ignore_time_macros`: `false`. If true, ignore `__DATE__`, `__TIME__` and `__TIMESTAMP__` being present in the source code. Will speed up preprocessor cache mode, but can produce stale results.

- `skip_system_headers`: `false`. If true, the preprocessor cache will only add the paths of included system headers to the cache key but ignore the headers' contents.

- `hash_working_directory`: `true`. If true, will add the current working directory to the cache key to distinguish two compilations from different directories.

See where to write the config in [the configuration doc](Configuration.md).

## Read-only cache mode

By default, the local cache operates in read/write mode. The `SCCACHE_LOCAL_RW_MODE` environment variable can be set to `READ_ONLY` (or `READ_WRITE`) to modify this behavior.

You can use read-only mode to prevent sccache from writing new cache items to the disk. This can be useful, for example, if you want to use items that have already been cached, but not add new ones to the cache. 

Note that this feature is only effective if you already have items in your cache. Using this option on an empty cache will cause sccache to simply do nothing, just add overhead.
