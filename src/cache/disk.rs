// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::cache::{
    Cache, CacheMode, CacheRead, CacheStorage, CacheWrite, GetPathResult, OpenCacheObject,
    PreprocessorCacheStorage,
};
use crate::compiler::PreprocessorCacheEntry;
use crate::lru_disk_cache::{Error as LruError, LruDiskCache, ReadSeek, Result as LruResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::ffi::OsStr;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::errors::*;

use super::lazy_disk_cache::LazyDiskCache;
use super::storage::put_packed_objects;
use super::utils::normalize_key;
use crate::config::PreprocessorCacheModeConfig;

/// A cache that stores entries at local disk paths.
pub struct DiskCache {
    /// `LruDiskCache` does all the real work here.
    lru: Arc<Mutex<LazyDiskCache>>,
    /// Thread pool to execute disk I/O
    pool: tokio::runtime::Handle,
    preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
    preprocessor_cache: Arc<Mutex<LazyDiskCache>>,
    rw_mode: CacheMode,
    basedirs: Vec<Vec<u8>>,
    reflink: bool,
}

impl DiskCache {
    /// Create a new `DiskCache` rooted at `root`, with `max_size` as the maximum cache size on-disk, in bytes.
    pub fn new<T: AsRef<OsStr>>(
        root: T,
        max_size: u64,
        pool: &tokio::runtime::Handle,
        preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
        rw_mode: CacheMode,
        basedirs: Vec<Vec<u8>>,
    ) -> DiskCache {
        DiskCache {
            lru: Arc::new(Mutex::new(LazyDiskCache::Uninit {
                root: root.as_ref().to_os_string(),
                max_size,
                use_weights: None,
            })),
            pool: pool.clone(),
            preprocessor_cache_mode_config,
            preprocessor_cache: Arc::new(Mutex::new(LazyDiskCache::Uninit {
                root: Path::new(root.as_ref())
                    .join("preprocessor")
                    .into_os_string(),
                max_size,
                use_weights: Some(false),
            })),
            rw_mode,
            basedirs,
            reflink: false,
        }
    }

    pub(super) fn with_reflink(mut self, requested: bool) -> Self {
        if requested {
            let lru = self.lru.lock().unwrap();
            let root = lru.path();
            match probe_reflink(root) {
                Ok(()) => self.reflink = true,
                Err(error) => warn!(
                    "Disk cache reflinks unavailable in {}: {error}",
                    root.display()
                ),
            }
        }
        self
    }

    async fn store(
        &self,
        key: &str,
        data: Bytes,
        has_raw_objects: Option<bool>,
    ) -> Result<Duration> {
        if self.rw_mode == CacheMode::ReadOnly {
            return Err(anyhow!("Cannot write to a read-only cache"));
        }

        let lru = self.lru.clone();
        let key = make_key_path(key);
        self.pool
            .spawn_blocking(move || {
                let start = Instant::now();
                store_bytes(&lru, key, data, has_raw_objects)?;
                Ok(start.elapsed())
            })
            .await?
    }
}

fn store_bytes(
    lru: &Mutex<LazyDiskCache>,
    key: PathBuf,
    data: Bytes,
    has_raw_objects: Option<bool>,
) -> Result<()> {
    let uses_weights =
        has_raw_objects != Some(false) && lru.lock().unwrap().get_or_init()?.uses_weights();
    let weight = if uses_weights
        && has_raw_objects.unwrap_or_else(|| {
            CacheRead::from(io::Cursor::new(data.clone()))
                .is_ok_and(|entry| entry.has_raw_objects())
        }) {
        estimate_weight(data.as_ref()).ok()
    } else {
        None
    };
    let mut entry = lru
        .lock()
        .unwrap()
        .get_or_init()?
        .prepare_add(key, weight.unwrap_or(data.len() as u64))?;
    let written = entry.as_file_mut().write_all(&data);
    let mut lru = lru.lock().unwrap();
    let lru = lru.get().unwrap();
    if let Err(error) = written {
        lru.cancel_add(entry);
        return Err(error.into());
    }
    Ok(lru.commit_weighted(entry, weight)?)
}

#[cfg(target_os = "linux")]
fn try_store_reflink(
    lru: &Mutex<LazyDiskCache>,
    key: &Path,
    objects: &[OpenCacheObject],
    stdout: &[u8],
    stderr: &[u8],
) -> Result<bool> {
    use std::io::Seek;

    let (mut entry, uses_weights) = {
        let mut lru = lru.lock().unwrap();
        let lru = lru.get_or_init()?;
        (lru.prepare_add(key, 0)?, lru.uses_weights())
    };
    let file = entry.as_file_mut();
    let written = CacheWrite::try_reflink_objects(file, objects, stdout, stderr);
    let weight = if matches!(written, Ok(true)) && uses_weights && !objects.is_empty() {
        file.rewind().and_then(|()| estimate_weight(file)).ok()
    } else {
        None
    };
    let mut lru = lru.lock().unwrap();
    let lru = lru.get().unwrap();
    if matches!(written, Ok(true)) {
        lru.commit_weighted(entry, weight)?;
    } else {
        lru.cancel_add(entry);
    }
    written
}

fn estimate_weight(mut data: impl Read) -> io::Result<u64> {
    const EXTENT: usize = 128 * 1024;
    const SECTOR: usize = 4096;

    let mut zstd = zstd::bulk::Compressor::new(1)?;
    let mut compressed = Vec::with_capacity(zstd::zstd_safe::compress_bound(EXTENT));
    let mut chunk = Vec::with_capacity(EXTENT);
    let mut weight = 0;
    // Chunk the ZIP from offset zero so its headers and raw data share the same
    // boundaries as they do in the file being compressed by btrfs.
    while data.by_ref().take(EXTENT as u64).read_to_end(&mut chunk)? > 0 {
        let len = zstd.compress_to_buffer(&chunk, &mut compressed)?;
        weight += len
            .next_multiple_of(SECTOR)
            .min(chunk.len().next_multiple_of(SECTOR));
        chunk.clear();
    }
    Ok(weight as u64)
}

#[cfg(target_os = "linux")]
pub(super) fn probe_reflink(root: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    fs_err::create_dir_all(root)?;
    let mut source = tempfile::NamedTempFile::new_in(root)?;
    source.write_all(&[0; 4096])?;
    let destination = tempfile::NamedTempFile::new_in(root)?;
    // SAFETY: FICLONE takes a source fd by value. Both files are open for the call.
    let result = unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn probe_reflink(_root: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Open the entry at `key`, leaving its file times alone in a read-only cache (see `get_untouched`).
fn open_entry<K: AsRef<OsStr>>(
    cache: &mut LruDiskCache,
    key: K,
    rw_mode: CacheMode,
) -> LruResult<fs_err::File> {
    match rw_mode {
        CacheMode::ReadOnly => cache.get_file_untouched(key),
        CacheMode::ReadWrite => cache.get_file(key),
    }
}

/// Make a path to the cache entry with key `key`.
fn make_key_path(key: &str) -> PathBuf {
    Path::new(&key[0..1]).join(&key[1..2]).join(key)
}

/// The path of the preprocessor cache entry for `key`: `normalize_key`'s layout, joined with
/// the platform separator, because a cache loaded from disk indexes entries by their relative
/// path as the platform spells it.
fn preprocessor_key_path(key: &str) -> PathBuf {
    normalize_key(key).split('/').collect()
}

#[async_trait]
impl CacheStorage for DiskCache {
    async fn get(&self, key: &str) -> Result<Cache> {
        trace!("DiskCache::get({})", key);
        let path = make_key_path(key);
        let lru = self.lru.clone();
        let key = key.to_owned();
        let rw_mode = self.rw_mode;
        let reflink = self.reflink;

        self.pool
            .spawn_blocking(move || {
                let io = match open_entry(lru.lock().unwrap().get_or_init()?, &path, rw_mode) {
                    Ok(f) => f,
                    Err(LruError::FileNotInCache) => {
                        trace!("DiskCache::get({}): FileNotInCache", key);
                        return Ok(Cache::Miss);
                    }
                    Err(LruError::Io(e)) => {
                        trace!("DiskCache::get({}): IoError: {:?}", key, e);
                        return Err(e.into());
                    }
                    Err(_) => unreachable!(),
                };
                let hit = CacheRead::from_file(io, reflink)?;
                Ok(Cache::Hit(hit))
            })
            .await?
    }

    async fn get_with_raw(&self, key: &str) -> Result<(Cache, Option<Bytes>)> {
        match self.get_raw(key).await? {
            Some(data) => {
                let hit = CacheRead::from(std::io::Cursor::new(data.clone()))?;
                Ok((Cache::Hit(hit), Some(data)))
            }
            None => Ok((Cache::Miss, None)),
        }
    }

    async fn get_raw(&self, key: &str) -> Result<Option<Bytes>> {
        trace!("DiskCache::get_raw({})", key);
        let path = make_key_path(key);
        let lru = self.lru.clone();
        let key = key.to_owned();
        let rw_mode = self.rw_mode;

        self.pool
            .spawn_blocking(move || {
                match open_entry(lru.lock().unwrap().get_or_init()?, &path, rw_mode) {
                    Ok(mut io) => {
                        let mut data = Vec::new();
                        io.read_to_end(&mut data)?;
                        trace!("DiskCache::get_raw({}): Found {} bytes", key, data.len());
                        Ok(Some(Bytes::from(data)))
                    }
                    Err(LruError::FileNotInCache) => {
                        trace!("DiskCache::get_raw({}): FileNotInCache", key);
                        Ok(None)
                    }
                    Err(LruError::Io(e)) => {
                        trace!("DiskCache::get_raw({}): IoError: {:?}", key, e);
                        Err(e.into())
                    }
                    Err(_) => unreachable!(),
                }
            })
            .await?
    }

    async fn get_path(&self, key: &str) -> GetPathResult {
        let rel_path = make_key_path(key);
        let lru = self.lru.clone();
        self.pool
            .spawn_blocking(move || {
                match lru
                    .lock()
                    .unwrap()
                    .get_or_init()
                    .ok()
                    .and_then(|c| c.get_abs_path(&rel_path))
                {
                    Some(p) => GetPathResult::Found(p),
                    None => GetPathResult::Miss,
                }
            })
            .await
            .unwrap_or(GetPathResult::Miss)
    }

    async fn put(&self, key: &str, entry: CacheWrite) -> Result<Duration> {
        trace!("DiskCache::put({})", key);
        let has_raw_objects = entry.has_raw_objects();
        let data = entry.finish()?;
        self.store(key, data.into(), Some(has_raw_objects)).await
    }

    async fn put_objects(
        &self,
        key: &str,
        objects: Vec<OpenCacheObject>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        pool: &tokio::runtime::Handle,
    ) -> Result<Duration> {
        #[cfg(target_os = "linux")]
        if self.reflink {
            if self.rw_mode == CacheMode::ReadOnly {
                bail!("Cannot write to a read-only cache");
            }
            let start = Instant::now();
            let lru = self.lru.clone();
            let key = make_key_path(key);
            return self
                .pool
                .spawn_blocking(move || {
                    if !try_store_reflink(&lru, &key, &objects, &stdout, &stderr)? {
                        let mut entry = CacheWrite::pack_objects(objects, true)?;
                        let has_raw_objects = entry.has_raw_objects();
                        entry.put_stdout(&stdout)?;
                        entry.put_stderr(&stderr)?;
                        store_bytes(&lru, key, entry.finish()?.into(), Some(has_raw_objects))?;
                    }
                    Ok(start.elapsed())
                })
                .await?;
        }
        put_packed_objects(self, key, objects, stdout, stderr, pool).await
    }

    fn write_raw_objects(&self) -> bool {
        self.reflink
    }

    async fn put_raw(&self, key: &str, data: Bytes) -> Result<Duration> {
        trace!("DiskCache::put_raw({}, {} bytes)", key, data.len());
        self.store(key, data, (!self.reflink).then_some(false))
            .await
    }

    async fn check(&self) -> Result<CacheMode> {
        self.lru.lock().unwrap().detect_weights();
        Ok(self.rw_mode)
    }

    fn location(&self) -> String {
        format!("Local disk: {:?}", self.lru.lock().unwrap().path())
    }

    fn cache_type_name(&self) -> &'static str {
        "disk"
    }

    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(self.lru.lock().unwrap().get().map(|l| l.size()))
    }
    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(Some(self.lru.lock().unwrap().capacity()))
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        &self.basedirs
    }
}

#[async_trait]
impl PreprocessorCacheStorage for DiskCache {
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        self.preprocessor_cache_mode_config
    }
    async fn get_preprocessor_cache_entry(&self, key: &str) -> Result<Option<Box<dyn ReadSeek>>> {
        let key = preprocessor_key_path(key);
        Ok(open_entry(
            self.preprocessor_cache.lock().unwrap().get_or_init()?,
            key,
            self.rw_mode,
        )
        .ok()
        .map(|file| Box::new(file) as Box<dyn ReadSeek>))
    }
    async fn put_preprocessor_cache_entry(
        &self,
        key: &str,
        preprocessor_cache_entry: PreprocessorCacheEntry,
    ) -> Result<()> {
        if self.rw_mode == CacheMode::ReadOnly {
            return Err(anyhow!("Cannot write to a read-only cache"));
        }

        let key = preprocessor_key_path(key);
        let mut f = self
            .preprocessor_cache
            .lock()
            .unwrap()
            .get_or_init()?
            .prepare_add(key, 0)?;
        let written = preprocessor_cache_entry.serialize_to(BufWriter::new(f.as_file_mut()));
        let mut lru = self.preprocessor_cache.lock().unwrap();
        let lru = lru.get().unwrap();
        if let Err(error) = written {
            lru.cancel_add(f);
            return Err(error.into());
        }
        Ok(lru.commit(f)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_cache_type_name() {
        let tempdir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let disk = DiskCache::new(
            tempdir.path(),
            1024 * 1024,
            runtime.handle(),
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadWrite,
            vec![],
        );

        assert_eq!(disk.cache_type_name(), "disk");
        assert!(!disk.write_raw_objects());
    }

    #[test]
    fn raw_weight_estimate_counts_incompressible_extents_at_full_length() {
        use rand::{RngCore, SeedableRng};

        let mut bytes = vec![0; 1024 * 1024];
        rand::rngs::StdRng::seed_from_u64(7).fill_bytes(&mut bytes);
        assert_eq!(
            estimate_weight(bytes.as_slice()).unwrap(),
            bytes.len() as u64
        );
    }

    #[test]
    fn raw_weight_estimate_rounds_zero_extents_to_one_sector_each() {
        assert_eq!(
            estimate_weight(&vec![0; 1024 * 1024 + 1][..]).unwrap(),
            9 * 4096
        );
    }

    const KEY: &str = "0123456789abcdef";

    fn disk_cache(root: &Path, runtime: &tokio::runtime::Runtime, rw_mode: CacheMode) -> DiskCache {
        DiskCache::new(
            root,
            1024 * 1024,
            runtime.handle(),
            PreprocessorCacheModeConfig::default(),
            rw_mode,
            vec![],
        )
    }

    #[test]
    fn test_disk_cache_reflink_probe_failure_keeps_compression() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("not-a-directory");
        std::fs::write(&root, b"").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let disk = disk_cache(&root, &runtime, CacheMode::ReadWrite).with_reflink(true);
        assert!(!disk.write_raw_objects());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn raw_cache_entry_store_reflink_failure_falls_back() {
        use crate::cache::FileObjectSource;
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::NamedTempFile::new_in("/dev/shm")
            .or_else(|_| tempfile::NamedTempFile::new_in("/tmp"))
            .unwrap();
        if other.as_file().metadata().unwrap().dev() == dir.path().metadata().unwrap().dev() {
            eprintln!("skipping cross-filesystem store fallback check: same device");
            return;
        }
        let local = dir.path().join("output");
        std::fs::write(&local, [1; 4096]).unwrap();
        std::fs::write(other.path(), [2; 4097]).unwrap();
        let sources = [local, other.path().to_owned()].map(|path| FileObjectSource {
            key: path.file_name().unwrap().to_str().unwrap().to_owned(),
            path,
            optional: false,
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut disk = disk_cache(&dir.path().join("cache"), &runtime, CacheMode::ReadWrite);
        disk.reflink = true;
        runtime.block_on(async {
            let packed = CacheWrite::from_objects(sources.clone(), runtime.handle(), true)
                .await
                .unwrap()
                .finish()
                .unwrap();
            disk.put_objects(
                KEY,
                CacheWrite::open_objects(sources).unwrap(),
                vec![],
                vec![],
                runtime.handle(),
            )
            .await
            .unwrap();
            assert_eq!(disk.get_raw(KEY).await.unwrap().unwrap(), packed);
            assert_eq!(
                disk.current_size().await.unwrap(),
                Some(packed.len() as u64)
            );
            assert_eq!(
                walkdir::WalkDir::new(disk.lru.lock().unwrap().path())
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.file_type().is_file())
                    .count(),
                2
            );
        });
        eprintln!("verified cross-filesystem fallback, archive bytes and LRU size");
    }

    enum Stored {
        CompileEntry,
        PreprocessorEntry,
    }

    /// A cache directory holding one entry under `KEY`, written by a read-write
    /// cache and then backdated, so that any refresh of its times by a later
    /// read is visible.
    struct ReadOnlyFixture {
        dir: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
        entry: PathBuf,
        old: filetime::FileTime,
    }

    fn read_only_fixture(stored: Stored) -> ReadOnlyFixture {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let writer = disk_cache(dir.path(), &runtime, CacheMode::ReadWrite);
        let entry = match stored {
            Stored::CompileEntry => {
                runtime
                    .block_on(writer.put(KEY, CacheWrite::new()))
                    .unwrap();
                dir.path().join(make_key_path(KEY))
            }
            Stored::PreprocessorEntry => {
                runtime
                    .block_on(
                        writer.put_preprocessor_cache_entry(KEY, PreprocessorCacheEntry::new()),
                    )
                    .unwrap();
                dir.path()
                    .join("preprocessor")
                    .join(preprocessor_key_path(KEY))
            }
        };
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        filetime::set_file_times(&entry, old, old).unwrap();
        ReadOnlyFixture {
            dir,
            runtime,
            entry,
            old,
        }
    }

    impl ReadOnlyFixture {
        fn reader(&self) -> DiskCache {
            disk_cache(self.dir.path(), &self.runtime, CacheMode::ReadOnly)
        }

        fn assert_untouched(&self) {
            let metadata = std::fs::metadata(&self.entry).unwrap();
            assert_eq!(
                filetime::FileTime::from_last_modification_time(&metadata),
                self.old
            );
        }
    }

    #[test]
    fn test_read_only_get_leaves_entry_untouched() {
        let f = read_only_fixture(Stored::CompileEntry);
        let hit = f.runtime.block_on(f.reader().get(KEY)).unwrap();
        assert!(matches!(hit, Cache::Hit(_)));
        f.assert_untouched();
    }

    #[test]
    fn test_read_only_get_raw_leaves_entry_untouched() {
        let f = read_only_fixture(Stored::CompileEntry);
        let reader = f.reader();
        let data = f.runtime.block_on(reader.get_raw(KEY)).unwrap();
        assert_eq!(data.as_deref(), Some(&std::fs::read(&f.entry).unwrap()[..]));
        let (hit, _) = f.runtime.block_on(reader.get_with_raw(KEY)).unwrap();
        assert!(matches!(hit, Cache::Hit(_)));
        f.assert_untouched();
    }

    #[test]
    fn test_read_only_preprocessor_entry_leaves_entry_untouched() {
        let f = read_only_fixture(Stored::PreprocessorEntry);
        let found = f
            .runtime
            .block_on(f.reader().get_preprocessor_cache_entry(KEY))
            .unwrap();
        assert!(found.is_some());
        f.assert_untouched();
    }

    #[test]
    fn test_preprocessor_entry_written_by_another_cache_is_found() {
        // A second cache on the same directory loads its index from disk, as an sccache
        // server started later does.
        let f = read_only_fixture(Stored::PreprocessorEntry);
        let reader = disk_cache(f.dir.path(), &f.runtime, CacheMode::ReadWrite);
        let found = f
            .runtime
            .block_on(reader.get_preprocessor_cache_entry(KEY))
            .unwrap();
        assert!(found.is_some());
    }
}
