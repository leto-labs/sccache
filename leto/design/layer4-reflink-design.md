# Layer 4.2 design: serving disk-cache hits with reflinks

Status: draft, 2026-11-20. Plan: `core/docs/infrastructure/2026-11-20-rust-build-master-plan.md`,
layer 4. Constraints from the layer 3 review (`layer3-incremental-review.md`, F2/R2).

## Goal

On a filesystem with reflinks (btrfs on the workers), a disk-cache hit should
share the cached object's extents with the output file in `target/` instead of
decompressing and writing a copy:

- ten worktrees that hit the same rlib share one set of extents (and, under a
  `zstd:1` cache folder, those extents stay compressed);
- a hit writes no object data.

Elsewhere (ext4 on devbox, tmpfs, another filesystem than the cache) nothing
changes.

## Today

A disk-cache entry is one zip file per key (`src/cache/disk.rs`,
`make_key_path`). Each object is stored as a zip member declared `Stored` whose
bytes are a zstd stream (`CacheWrite::put_object`, `src/cache/cache_io.rs`). A
hit opens the file, and `CacheRead::extract_objects` decompresses each member
into a `NamedTempFile` next to the output and renames it into place.

Reflinks need the object's bytes as they are, at a block-aligned offset in the
cache file. zstd streams rule that out.

## Design

### Entry format: aligned raw members, one file per entry

- With the setting on, `CacheWrite` writes each object **uncompressed**, with
  its data start aligned to 4096 bytes (`SimpleFileOptions::with_alignment`,
  zip 9; the padding goes into the local header's extra field), under the
  member name `raw/<key>`. stdout/stderr stay zstd members as today.
- `CacheRead::get_object(name)` looks up `name` (zstd, as today) and then
  `raw/<name>` (copied as is). Entries of both formats can live in one cache.
- The entry is still a single zip file, so `LruDiskCache`, `get_raw`/`put_raw`,
  multilevel back-fill and the read-only mode are unchanged.
- An older sccache reading a new entry does not find `<key>`, gets
  `DecompressionFailure`, and treats the hit as a miss
  (`compiler.rs`, `MissType::CacheReadError`). A rollback costs misses, not
  failures.

Alignment of the *length* is not possible inside a zip (members follow each
other), so a hit clones the aligned prefix of each object and writes the
remaining tail (< 4096 bytes) normally (below).

### Hit path

1. `DiskCache::get` opens the entry as a `std::fs::File` and keeps a duplicate
   handle in the `CacheRead` (new optional field). Other backends pass no file.
2. For each `raw/` member with a source file:
   - `off = data_start`, `len = size`, `head = len & !4095`;
   - create the `NamedTempFile` in the output's folder (as today);
   - if `head > 0`: `ioctl(tmp, FICLONERANGE, {src_fd, src_offset: off,
     src_length: head, dest_offset: 0})`;
   - copy `len - head` bytes from `off + head` with `pread`/`write`;
   - set the mode, then `persist` (rename) onto the output.
3. Any clone error (`EXDEV`, `EOPNOTSUPP`, `EINVAL`, `ENOTTY`, `ETXTBSY`,
   `ENOSPC` …) falls back to a plain copy of the member (no decompression),
   once per entry, and the first failure per server is logged at info.

The destination is always a new inode created in the output folder, then
renamed: the layer 3 rule R2 (rustc hard-links `deps/lib*.rmeta` into its
incremental session, so writing through an existing inode corrupts the
session). Never `FICLONE` onto an existing output, and never hard-link
between the cache and `target/` in either direction.

Cloning into a file that is mapped or executed elsewhere is not an issue: the
temp file is new.

### Store path

Unchanged except the member format: the entry is built in memory and written
with `put_raw`. A later step can clone the outputs into the entry file instead
of copying their bytes (rustc just wrote them); not needed for the goal, since
store cost is off the critical path.

### Setting

`[cache.disk] reflink = true|false` (env `SCCACHE_DISK_REFLINK`), default
false. At start-up with the setting on, the server probes the cache folder:
create two files there, `FICLONE` one onto the other. If the probe fails, log
a warning and write compressed entries (the setting then has no effect), so
an ext4 host never stores uncompressed entries by accident.

Applies to the disk cache only, including a disk level of a multilevel cache
only if that level is the one that builds the entry. A remote level would
upload uncompressed entries, which is wasteful; v1 enables the format only
when the storage is a plain `DiskCache`.

### Sizes and accounting

- Raw members are larger than zstd members: about 4× for rlibs (Longbow
  sample: `deps/` 964 → 222 MiB at zstd 1). On a `zstd:1` btrfs folder the
  filesystem compresses them again, per 128 KiB extent, so the bytes on disk
  end up near today's.
- `LruDiskCache` counts apparent file sizes. A 300 GiB limit therefore holds
  roughly a quarter as many entries in raw format. Options: raise the limit
  for reflink caches (disk use is what `compsize` reports, not apparent size),
  or count allocated size. btrfs reports `st_blocks` uncompressed, so allocated
  size does not help; raise the limit and watch `compsize`. To be measured.
- Extents shared with worktrees are freed only when the last reference goes:
  evicting an entry does not free the bytes while a worktree holds a clone,
  and deleting a worktree does not free bytes the cache holds. Each byte is
  stored once either way.

### What it does not cover

- Linked outputs (test harnesses, binaries, proc-macros) are not cached, so
  they never come from a reflink. They are also not compressed by btrfs when
  mold links them (see "Linked outputs" below).
- Hits from a remote cache tier.

## Linked outputs on btrfs (from status 1)

mold preallocates its output (`fallocate`, `src/output-file-unix.cc:57` at
v2.40.4, unconditional for regular files, no option) and writes it through
`mmap`; btrfs writes into preallocated extents without compression. Measured
on worker-02: a 7.2 MB test binary stored at 100%, 27% after a plain copy.
Measured 2026-11-20 on worker-01 (btrfs RAID1, `zstd:1` folder), Longbow
`ec92f707d`, `test --workspace --no-run`, no sccache. Full build with mold,
then executables deleted and relinked with each variant (the relink step also
recompiles the bin and test crates). One run each, host load 10-15, so times
are indicative only.

| Variant | Executables, apparent | Executables, on disk | Whole `target/`, on disk | Relink step |
|---|---|---|---|---|
| mold (as configured) | 49.7 GB | 43.1 GB (0% compressed) | ~44.8 GB | 163 s |
| lld | 40.6 GB | 6.4 GB (ratio 0.19) | ~8.1 GB (-82%) | 126 s |
| mold, `fallocate` disabled (LD_PRELOAD shim) | 49.7 GB | 8.0 GB (ratio 0.19) | ~9.7 GB (-78%) | 145 s |
| mold, then `btrfs filesystem defragment -czstd` | 49.7 GB | 12.1 GB (ratio 0.28) | ~13.7 GB (-69%) | 163 s + 40 s |

Executables are 88% of this `target/` (Longbow test harnesses with
`debug = 1`); rlibs compress to 0.17 and rmeta to 0.40 with any linker.

Recommendation: **lld** for the layer 1 linker decision. It needs no patched
tool, its outputs compress normally on btrfs, they are 18% smaller even
uncompressed (which also helps ext4 on devbox), and it was not slower here.
A mold without `fallocate` gives similar disk use but needs a patched mold
build to maintain. Post-link defragmentation compresses less, writes every
byte twice and costs a separate pass. Before the switch, the project threads
should confirm link times in their own edit loop (one candidate run with
`-C link-arg=-fuse-ld=lld`), because the relink step above includes
compilation.

## Tests

Unit tests in `src/cache/cache_io.rs`:

1. Raw entry round trip without a source file (other backends): bytes equal,
   mode kept.
2. Raw entry with a source file on a filesystem without reflinks (tmpfs/ext4
   in CI): fallback copy, bytes equal.
3. Alignment: every `raw/` member's `data_start` is a multiple of 4096, for
   object sizes 0, 1, 4095, 4096, 4097 and 1 MiB + 7.
4. Mixed entry: zstd and raw members readable from one entry; an old-format
   entry still reads.
5. R2: the destination has a second hard link; after extraction the other name
   keeps its content and the destination has a new inode.
6. On btrfs only (skipped elsewhere, run on a worker): after a hit, `FIEMAP` of
   the output shows shared extents (`FIEMAP_EXTENT_SHARED`) for the aligned
   head, and the bytes are equal for the same size set as test 3.

End to end (`tests/sccache_cargo.rs`, skipped unless the temp folder supports
reflinks): two checkouts, `reflink = true`; B's rlibs equal A's byte for byte,
and the stats show no fallback.

## Open questions

1. Accounting: raise the size limit, or teach the LRU about raw entries?
2. Should C/C++ objects use the raw format too? Same code, small objects; a
   reflink saves little below one extent. Default: all objects, it is simpler.
3. Store-side clone from the outputs into the entry (removes the second data
   write on a miss): worth it after measurement?
