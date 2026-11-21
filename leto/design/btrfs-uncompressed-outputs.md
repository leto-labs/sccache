# Why btrfs build outputs reach the cache uncompressed

Measured on worker-01 (Longbow) and worker-02 (Markets-rs) on 2026-11-21.

## Ranked causes

1. **Missing compression at file creation explains the historical pre-warm and
   older-lane data.** The strongest example is a 212.3 MiB Markets-rs pre-warm
   `.rlib` with no compression property or `c` flag. Its one uncompressed
   extent also backs a raw sccache entry that was created immediately after
   rustc finished the library. The pre-warm directory properties observed
   during this investigation were applied after those outputs were created.
2. **Normal btrfs compression selection skips some highly compressible ranges
   even in new compressed lanes.** A fresh Markets-rs `.rlib` had a 90.9 MiB
   uncompressed extent that it shared with a newer cache entry. A fresh
   Longbow `.rlib` in a compressed lane shared a 128 MiB uncompressed range
   with the cache and several other lanes. On both hosts, ordinary buffered
   writes of 8 MiB from those ranges to new `compression=zstd` inodes stayed
   entirely uncompressed. Forcing compression on scratch copies reduced the
   on-disk size to 24.2% and 19.4%, respectively. Plain rustc reproduced the
   same behavior in a compressed scratch tree with sccache out of the loop.
   The kernel's heuristic is the leading explanation for those ranges. The
   experiment establishes a difference in the normal compression path and
   leaves the exact kernel branch uninstrumented.
3. **Mold preallocation explains uncompressed linked outputs where mold is
   used.** The scratch trace reproduced a single uncompressed extent from
   mold in a compressed directory. The same test linked with rust-lld used
   31–32% of its uncompressed extent size. Older Longbow worktree config
   explicitly selected mold. Both current pre-warm checkouts use the toolchain
   default rust-lld according to their Cargo configuration. The measurements
   cannot identify the linker that created each retained historical binary.
   Rustc's archive and metadata paths in the probe did no preallocation.
4. **sccache preserves and redistributes these extents.** A reflink store or
   restore keeps the existing extent encoding. The fork's copying paths use
   buffered file writes. Rustc, Cargo and sccache used output-local paths for
   the operations examined here. No target-dir or temporary-directory
   override was found that accounts for the sampled shared extents.

## Extent provenance and directory properties

Both hosts ran `7.0.0-43-generic`. The home subvolume is mounted btrfs with
`rw,noatime,ssd,discard=async,space_cache=v2,subvol=/@home` and no compression
mount option. `~/.cache` is on btrfs and has no compression property. The
cache root `~/.cache/sccache` has `compression=zstd:1`. Both `/tmp` and
`/var/tmp` are on the ext4 root filesystem. Sampled live Markets-rs Cargo
processes used `~/.cache/t3-agent-tmp`, which has no compression property.
The pre-warm configuration points `PREWARM_REPO` at each canonical
checkout, builds `main` in the dedicated clone, and has no
`CARGO_TARGET_DIR` or `TMPDIR` override. No target-dir override appeared in
the inspected shell, Cargo configs, wrappers or sampled live Cargo
environments. All sampled pre-warm artifacts were under
`~/.cache/leto-maintenance/prewarm/<project>/checkout/target/debug/deps`.

On both hosts the pre-warm root, project state, checkout, `target`, and
`debug/deps` now report `compression=zstd:1` and `lsattr -d` shows `c`.
The pre-warm root and nested directory ctimes cluster at 04:42 CET on
November 21, hours after the first output births. The installed runner on
both workers matches the devbox script at commit `1aa6179a`, which adds
`PREWARM_COMPRESSION` and sets the property on the project state and its
existing directories. The new script was installed at 04:45 CET. Existing
regular files and their allocated extents retain their previous state.
[Pre-warm runner](../prewarm/prewarm.py#L68) · [Pre-warm configuration](../prewarm/README.md#config-reference)

The canonical checkout, its target and its `debug/deps` on each host
currently have `zstd:1` or inherited `zstd` and `c`. Some older Markets-rs
canonical files, including `libarrow_array-c3f7c176c8e1cf32.rlib`, still
have neither. On each worker, the three newest checked populated
worktrees, their targets and `debug/deps` inherit `zstd` and `c`. The
sampled older worktree roots lack the property even though their
`target` and `debug/deps` now show `zstd:1` and `c`. The older Longbow
sample's large output inodes lack both the property and the flag. The
older Markets-rs sample contains both types of inode.

Read-only FIEMAP and `btrfs inspect-internal logical-resolve` showed the
same physical addresses in these target files and cache entries. Reading
the ZIP central directories confirmed the named `raw/` members. Times
below are local CET. The file length can include a final partial sector
beyond the cloned head.

| Worker and output | Shared uncompressed range | Cache key prefix | Observed chronology |
| --- | ---: | --- | --- |
| 02 pre-warm `libmarketlab_backtest-c5dfffbc3fc2c24b.rlib` | 212.3 MiB at offset 0 | `efc40c1f840d` | Property-free output completed 01:25:34.239. Cache inode born 01:25:34.462. |
| 02 pre-warm `libmarketlab_backtest-5a66ad947c9f06ea.rmeta` | 25.9 MiB at offset 4 KiB | `f056f6058960` | Property-free metadata written Nov 20 at 20:25:11.940. Cache inode born 20:25:51.967. The metadata also had a hard link in the checkout's incremental directory. |
| 02 fresh `libmarketlab_service-71f751b1dfdb3fe7.rlib` | 90.9 MiB at offset 0 | `a2d01f3643fd` | Output had inherited `zstd` and completed 04:34:56.649. Cache inode born 04:34:56.743. |
| 01 fresh `libaws_sdk_ec2-59c7645ce451d76c.rlib` | 128 MiB at offset 256 MiB | `0b03f18d1154` | A property-bearing lane output predates the cache inode by about two seconds. The cache then predates the sampled fresh worktree's copy. |
| 01 pre-warm `libaws_sdk_ec2-469af04cfcaa203f.rlib` | 6 MiB at offset 287 MiB | `cf1624e222e2` | The cache inode predates the property-free pre-warm copy by over six hours. This pre-warm sample is consistent with a cache hit. |

The original 80.5 MiB example was supplied without a path or extent
address. These independently identified extents establish both mechanisms.

## Bounded `compsize` samples

Each row uses 18 files directly under `target/debug/deps`. For each of
`.rlib`, nonempty `.rmeta`, and executable names without an extension,
the sample takes six positions spaced evenly by file-size rank, including
the smallest and largest. `sudo -n nice -n 19 ionice -c3 compsize -b`
counted each physical extent once per sample. Values are MiB. The
`Uncompressed` column is the expanded size of those extents, including
sector rounding. Reflinks can share them across rows. These bytes cannot
be added across samples to estimate total filesystem usage.
[compsize reporting semantics](https://github.com/kilobyte/compsize)

| Worker | Tree | Disk | Uncompressed | `none` | `zstd` on disk | `zstd` expanded | Disk / expanded |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 01 | Longbow fresh | 615.0 | 3,378.2 | 74.7 | 540.2 | 3,303.5 | 18.2% |
| 01 | Longbow older | 3,185.1 | 3,185.1 | 3,185.1 | 0 | 0 | 100% |
| 01 | Longbow pre-warm | 2,464.6 | 3,299.5 | 2,322.9 | 141.7 | 976.6 | 74.7% |
| 01 | Longbow canonical | 588.2 | 3,238.7 | 70.4 | 517.8 | 3,168.3 | 18.2% |
| 02 | Markets-rs fresh | 496.3 | 2,134.6 | 44.3 | 452.0 | 2,090.3 | 23.3% |
| 02 | Markets-rs older | 349.1 | 579.5 | 304.8 | 44.2 | 274.7 | 60.2% |
| 02 | Markets-rs pre-warm | 1,432.8 | 1,435.2 | 1,431.5 | 1.3 | 3.7 | 99.8% |
| 02 | Markets-rs canonical | 742.9 | 997.2 | 678.8 | 64.1 | 318.4 | 74.5% |

The highest target mtime at sampling time belonged to
`t3-736842ba-c4108d74c84694d4` on worker-01 and
`t3-f7de6060-ebd9f768183eb8fa` on worker-02. The former had
`release-native` and nested probe targets. The latter target was empty.
Both lacked a direct `target/debug/deps`, so the fresh rows use the next
newest targets by mtime with populated `debug/deps`:
`t3-f41b6b93-96ed175962e0d003` (04:08:27 CET) and
`t3-c994bbae-9dbd23c436c342af` (04:35:23 CET). The older rows use
`t3-02cf2765-f920fd3e24f78d09` and `t3-1271dec6-1b1cec9a3c232952`,
the oldest targets by mtime with eligible files. An older Longbow
`debug/deps` held none of the three sampled output types.

Longbow pre-warm is mainly a linked-output problem in this sample. The six
libraries used 14% of expanded size, the six metadata files used 32%, and
the six executables occupied 2,290.1 MiB, all `none`. The older Markets-rs
sample also has a fully uncompressed executable group (294.1 MiB) alongside
library and metadata groups at 16% and 43%. In Markets-rs pre-warm, all
three groups used at least 98% of expanded size.

## What the compiler and cache wrote

The probe used Rust 1.99.1 (`58cf8f9c3`, LLVM 23.1.1), a generated
dependency-free library with debug info, and a test harness. Scratch dirs
had `compression=zstd:1`. Cargo ran offline with an empty wrapper and a
scratch `CARGO_HOME`. `strace -f -yy` included `fallocate`, `ftruncate`,
`mmap`, `copy_file_range`, `ioctl`, opens, renames and hard links.

| Scratch output | Syscalls affecting the output | btrfs result |
| --- | --- | --- |
| Plain rustc `.rlib` | Archive writer creates `<out>/.tmp*.temp-archive/tmp.a` and renames it into place. The trace shows no output `fallocate`, `ftruncate`, writable shared mmap, clone ioctl or `copy_file_range`. | 2,142,208 bytes, one `none` extent on both hosts. A level-1 forced rewrite used 331,776 bytes. |
| Plain rustc `.rmeta` | Creates `<out>/rmeta*/full.rmeta` and renames it into place. With incremental enabled, rustc also hard-links the resulting inode into the incremental session. | 8,192 bytes on disk from 16,384 expanded, all `zstd`. The incremental probe left `.rlib` and `.rmeta` staging under the output directory even with a separate property-free scratch `TMPDIR`. |
| Test via `cc` and bundled rust-lld | Output-adjacent temp file, `ftruncate`, writable `MAP_SHARED`, no `fallocate`. | Roughly 2.33 MiB on disk from 7.39 MiB expanded, including 0.375 MiB `none`. Distro lld behaved similarly. |
| Same test via `clang -fuse-ld=mold` | Output-adjacent temp file, `ftruncate`, successful `fallocate`, writable `MAP_SHARED`, then rename. | 7,770,112 bytes, one `none` extent. |

`filefrag -v` showed `encoded` on compressed extents and one plain
extent each for the generated archive and mold output. A 1 MiB repeated
pattern was 32 KiB on disk via buffered write and via `ftruncate` plus
writable mmap. With successful preallocation the mmap version used the
whole 1 MiB. Defrag of a scratch copy brought that back to 32 KiB.
A separate `cp --reflink=always` control cloned a plain source into a
new file with inherited `c`. The destination stayed entirely `none`,
and `filefrag` showed the same physical range marked `shared` on both.
Plain Cargo uplifted the scratch library via `linkat`. Both output
names referred to the same inode. [Rust archive writer](https://github.com/rust-lang/rust/blob/58cf8f9c35b355199727627d07ae34a4e2846d67/compiler/rustc_codegen_ssa/src/back/archive.rs#L675)

- The server-side disk store creates an LRU tempfile inside the cache
  root. `try_reflink_objects` writes ZIP headers and tail bytes,
  clones the aligned payload with `FICLONERANGE`, and seeks over that
  payload as the ZIP writer computes CRCs. Neither the file creation
  nor the raw pack calls `set_len` or `fallocate`. The reservation in
  `prepare_add` is LRU accounting. [Disk store](../../src/cache/disk.rs#L152) · [Raw reflink store](../../src/cache/cache_io.rs#L665) · [LRU tempfile](../../src/lru_disk_cache/mod.rs#L408)
- In this checkout, the client-side store packs the object bytes in
  memory and sends `StoragePutRaw`. The daemon uses `write_all` for
  that buffer. An older daemon can ask the client to use compressed
  object members instead. The shared extents above prove a clone
  occurred for those specific entries. The configured pre-warm
  default of `SCCACHE_CLIENT_SIDE=1` does not establish which path
  historical invocations took. [Pack path](../../src/cache/storage.rs#L396) · [Client IPC](../../src/cache/ipc_storage.rs#L122) · [Daemon buffered store](../../src/cache/disk.rs#L119)
- Restores create a tempfile in the destination's parent and persist
  it there. Raw fallback uses buffered copying, and compressed
  members decode into the destination. `set_len(0)` only clears a
  failed reflink before fallback. The direct-create fallback also
  writes to a fresh inode in the destination directory. [Restore path](../../src/cache/cache_io.rs#L249)

## Compression benefit and CPU cost

For each selected uncompressed FIEMAP range, samples used up to 8 MiB of
whole 128 KiB blocks. Each block was compressed independently with
libzstd level 1. The sector estimate rounds to 4 KiB and stores the
original block if compression saves no sector. The btrfs check uses
`btrfs filesystem defragment -f -czstd -L 1` on a scratch copy of the
same bytes. Ordinary writes were separate user-space buffered writes
with no extent sharing with the production source. Differences
between the estimates and the btrfs results reflect chunk boundaries and
compression settings.

| Sample from uncompressed ranges | Sample MiB | zstd:1 payload / input | Sector estimate | Ordinary btrfs write | Forced btrfs copy |
| --- | ---: | ---: | ---: | ---: | ---: |
| 01 pre-warm `.rlib` | 6.0 | 80.5% | 82.0% | 100% | 82.0% |
| 01 pre-warm `.rmeta` | 5.5 | 83.0% | 84.3% | 100% | 84.3% |
| 01 pre-warm test executable | 8.0 | 14.6% | 16.3% | 16.0% | 16.3% |
| 01 older `.rlib` | 8.0 | 17.4% | 19.4% | 100% | 19.4% |
| 01 fresh `.rlib` | 8.0 | — | — | 100% | 19.4% |
| 02 pre-warm `.rlib` | 8.0 | 6.1% | 7.5% | 7.5% | 7.5% |
| 02 pre-warm `.rmeta` | 8.0 | 40.4% | 41.9% | 39.6% | 41.8% |
| 02 pre-warm executable | 8.0 | 17.2% | 19.0% | 18.7% | 19.0% |
| 02 older `.rlib` | 8.0 | 7.4% | 8.7% | 8.4% | 8.7% |
| 02 fresh `.rlib` | 8.0 | — | — | 100% | 24.2% |

Every independently compressed block in the listed pre-warm and older
`.rlib` samples saved at least one sector. In the first 16 blocks of
each fresh `.rlib` range, all 32 blocks saved a sector. The raw
ratios for those first 2 MiB were 38.2% (Longbow, 40.0% after rounding)
and 7.6% (Markets-rs, 9.0% after rounding). The first 128 KiB of the
generated archive compressed to 32,858 bytes. The forced btrfs copy
compressed all of its extents, evidence that normal compression
selection can miss good candidates even at offset zero.

Both workers have AMD EPYC 9454 processors with 48 cores and 96 hardware
threads. `zstd -b1 -e1 -i1 -B128K` measured the following single-thread
throughput in decimal MB/s from the pre-warm byte samples. This is a
userspace codec benchmark in memory on busy hosts. Kernel compression,
checksums, writeback and overall build effects were outside this timing.

| Worker | `.rlib` | `.rmeta` | Executable | Random 8 MiB |
| --- | ---: | ---: | ---: | ---: |
| 01 | 389 | 382 | 531 | 7,507 |
| 02 | 1,416 | 347 | 500 | 8,488 |

Build-data throughput of 347–1,416 MB/s implies roughly 0.8–3.1 core-seconds
of codec work per GiB submitted. Random input expanded by 768 bytes across
64 frames, which btrfs would discard. That easy rejection still cost about
0.13–0.14 core-seconds per GiB in the CLI benchmark. Other already encoded
or high-entropy inputs can take longer. Test-binary decompression was
about 1.4–1.5 GB/s in the CLI. Compressed extents can lower cold I/O volume
and add decompression work to those reads. Reads served by the page cache
avoid the codec. Build read latency was outside the benchmark.

The fresh-lane samples had only 2.1–2.2% uncompressed extent bytes by
expanded size. Those bytes account for 8.9% of sampled disk use in
Markets-rs and 12.2% in Longbow, which bounds the additional space that
force could save on these samples. If all that remaining `none` data
behaved like the tested 8 MiB fresh library ranges, the incremental
gain would be about 7% and 10% of sampled current disk use, respectively.
This is a sensitivity calculation. Existing stale or preallocated
extents and the choices behind the small sample preclude a fleet-wide
savings estimate.

## Recommended fix

Keep the pre-warm property retrofit and verify inheritance on newly
written outputs after the next normal build. Keep the current rust-lld
default on both projects. Retire stale property-free build outputs and
cache entries through normal target cleanup and cache eviction so
rebuilds can populate new extents. If a lane uses a custom target
root, set the property on that root before creating its build tree.
The observed archive and metadata paths give no reason to change
`TMPDIR` for this issue.

Defer `compress-force=zstd:1` for `/home`. Consider it if a
post-retrofit sample still contains enough skipped, compressible data
to justify added write CPU across the filesystem. In upstream Linux 7.0
the property enables compression selection and suppresses sticky
`NOCOMPRESS` after a failed compression trial. The selection still calls
the heuristic. Force bypasses that heuristic on eligible CoW writes,
and a successful encode still needs to save at least one sector. A
failed encode can leave the rest of that async chunk uncompressed.
Btrfs can route preallocated writes ahead of that decision, so the mount
option does not fix mold's preallocation behavior. The property parser
selects the algorithm and inherits `zstd`; the upstream v7.0 write
path obtains the default level from the filesystem. The explicit
level-1 probes used defrag and zstd, and a `compress-force=zstd:1`
mount option would select level 1 for eligible future writes.
[Linux 7.0 compression selection](https://github.com/torvalds/linux/blob/v7.0/fs/btrfs/inode.c#L795) · [Property handling](https://github.com/torvalds/linux/blob/v7.0/fs/btrfs/props.c#L304) · [Btrfs compression documentation](https://btrfs.readthedocs.io/en/latest/Compression.html)

Changing the mount option leaves existing extents in place. A reflink
can still share compressed and uncompressed source extents. Avoid
periodic `btrfs filesystem defragment -czstd` over the cache.
**Defragmentation unshares reflinked extents.** The cache could write a
new compressed extent while old full-sized extents remain pinned by
live targets, increasing the combined disk usage.
[Btrfs defragmentation documentation](https://btrfs.readthedocs.io/en/latest/Defragmentation.html)

All probe directories were removed and no server RPCs were issued.
The compiler outputs and forced rewrites were confined to scratch.
The cc driver used its default transient GCC linker bookkeeping path
under `/tmp` in the default-`TMPDIR` traces. The driver cleaned those
paths and they were absent at the final check.
