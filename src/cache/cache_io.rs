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

use super::utils::{get_file_mode, set_file_mode};
use crate::errors::*;
use fs_err as fs;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zip::read::{ArchiveOffset, Config as ZipReadConfig, ZipFile};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

const RAW_ALIGNMENT: u16 = 4096;

/// Cache object sourced by a file.
#[derive(Clone)]
pub struct FileObjectSource {
    /// Identifier for this object. Should be unique within a compilation unit.
    /// Note that a compilation unit is a single source file in C/C++ and a crate in Rust.
    pub key: String,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Whether the file must be present on disk and is essential for the compilation.
    pub optional: bool,
}

/// Result of a cache lookup.
pub enum Cache {
    /// Result was found in cache.
    Hit(CacheRead),
    /// Result was not found in cache.
    Miss,
    /// Do not cache the results of the compilation.
    None,
    /// Cache entry should be ignored, force compilation.
    Recache,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Cache::Hit(_) => write!(f, "Cache::Hit(...)"),
            Cache::Miss => write!(f, "Cache::Miss"),
            Cache::None => write!(f, "Cache::None"),
            Cache::Recache => write!(f, "Cache::Recache"),
        }
    }
}

/// CacheMode is used to represent which mode we are using.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheMode {
    /// Only read cache from storage.
    ReadOnly,
    /// Full support of cache storage: read and write.
    ReadWrite,
}

/// Trait objects can't be bounded by more than one non-builtin trait.
pub trait ReadSeek: Read + Seek + Send {}

impl<T: Read + Seek + Send> ReadSeek for T {}

/// Data stored in the compiler cache.
pub struct CacheRead {
    zip: ZipArchive<Box<dyn ReadSeek>>,
    #[cfg(target_os = "linux")]
    reflink_source: Option<File>,
}

/// Represents a failure to decompress stored object data.
#[derive(Debug)]
pub struct DecompressionFailure;

impl std::fmt::Display for DecompressionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decompress content")
    }
}

impl std::error::Error for DecompressionFailure {}

impl CacheRead {
    pub(super) fn has_raw_objects(&self) -> bool {
        self.zip
            .file_names()
            .flatten()
            .any(|name| name.starts_with("raw/"))
    }

    /// Create a cache entry from `reader`.
    pub fn from<R>(reader: R) -> Result<CacheRead>
    where
        R: ReadSeek + 'static,
    {
        // Cache entries are standalone ZIP archives written by CacheWrite, so the
        // archive always starts at byte 0. Avoid zip's generic embedded-archive scan.
        let config = ZipReadConfig {
            archive_offset: ArchiveOffset::Known(0),
        };
        let z = ZipArchive::with_config(config, Box::new(reader) as Box<dyn ReadSeek>)
            .context("Failed to parse cache entry")?;
        Ok(CacheRead {
            zip: z,
            #[cfg(target_os = "linux")]
            reflink_source: None,
        })
    }

    pub(super) fn from_file(reader: fs::File, _reflink: bool) -> Result<Self> {
        #[cfg(target_os = "linux")]
        if _reflink {
            let source = match reader.file().try_clone() {
                Ok(file) => Some(file),
                Err(error) => {
                    log_reflink_failure(&error);
                    None
                }
            };
            let mut entry = Self::from(reader)?;
            entry.reflink_source = source;
            return Ok(entry);
        }
        Self::from(reader)
    }

    /// Get an object from this cache entry at `name` and write it to `to`.
    /// If the file has stored permissions, return them.
    pub fn get_object<T>(&mut self, name: &str, to: &mut T) -> Result<Option<u32>>
    where
        T: Write,
    {
        let (file, raw) = Self::object_member(&mut self.zip, name)?;
        Self::copy_object(file, raw, to)
    }

    fn object_member<'a>(
        zip: &'a mut ZipArchive<Box<dyn ReadSeek>>,
        name: &str,
    ) -> Result<(ZipFile<'a, Box<dyn ReadSeek>>, bool)> {
        let (index, raw) = Self::object_index(zip, name).ok_or(DecompressionFailure)?;
        let file = zip.by_index(index).or(Err(DecompressionFailure))?;
        if file.compression() != CompressionMethod::Stored
            || (raw && file.size() != file.compressed_size())
        {
            bail!(DecompressionFailure);
        }
        Ok((file, raw))
    }

    fn object_index(zip: &ZipArchive<Box<dyn ReadSeek>>, name: &str) -> Option<(usize, bool)> {
        zip.index_for_name(name)
            .map(|index| (index, false))
            .or_else(|| {
                zip.index_for_name(&format!("raw/{name}"))
                    .map(|index| (index, true))
            })
    }

    fn copy_object<T: Write>(
        mut file: ZipFile<'_, Box<dyn ReadSeek>>,
        raw: bool,
        to: &mut T,
    ) -> Result<Option<u32>> {
        let mode = file.unix_mode();
        if raw {
            io::copy(&mut file, to).or(Err(DecompressionFailure))?;
        } else {
            zstd::stream::copy_decode(file, to).or(Err(DecompressionFailure))?;
        }
        Ok(mode)
    }

    fn extract_object(&mut self, name: &str, output: &mut File) -> Result<Option<u32>> {
        let (file, raw) = Self::object_member(&mut self.zip, name)?;
        #[cfg(target_os = "linux")]
        if raw && let Some(source) = &self.reflink_source {
            let size = file.size();
            let head = size & !(u64::from(RAW_ALIGNMENT) - 1);
            if head > 0
                && let Some(offset) = file.data_start()
            {
                match reflink_object(source, output, offset, head, size - head) {
                    Ok(()) => return Ok(file.unix_mode()),
                    Err(error) => {
                        log_reflink_failure(&error);
                        self.reflink_source = None;
                        output.set_len(0)?;
                    }
                }
            }
        }
        Self::copy_object(file, raw, output)
    }

    /// Get the stdout from this cache entry, if it exists.
    pub fn get_stdout(&mut self) -> Vec<u8> {
        self.get_bytes("stdout")
    }

    /// Get the stderr from this cache entry, if it exists.
    pub fn get_stderr(&mut self) -> Vec<u8> {
        self.get_bytes("stderr")
    }

    fn get_bytes(&mut self, name: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        drop(self.get_object(name, &mut bytes));
        bytes
    }

    pub async fn extract_objects<T>(
        mut self,
        objects: T,
        pool: &tokio::runtime::Handle,
    ) -> Result<()>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
            for FileObjectSource {
                key,
                path,
                optional,
            } in objects
            {
                if is_path_null(&path) {
                    // For unix, this is just a fast path to discard such outputs,
                    // so it is not an issue if `is_path_null` has false-negatives.
                    // But for Windows, since `NUL` looks like a relative path, the
                    // temporary file creation logic would happily succeed, creating
                    // a temp file in the CWD, but then the subsequent `persist`
                    // would fail with `ERROR_ALREADY_EXISTS`, since `NUL` always
                    // exists and cannot be `replaced`, so we really need to
                    // short-circuit more of such cases on Windows.
                    debug!("Skipping output to {}", path.display());
                    continue;
                }
                #[cfg(unix)]
                if is_output_stream(&path) {
                    let mut stream = open_output_stream(&path)?;
                    let result = self.get_object(&key, &mut stream);
                    if !optional {
                        result?;
                    }
                    continue;
                }
                let dir = match path.parent() {
                    Some(d) => d,
                    None => bail!("Output file without a parent directory!"),
                };
                // Write the cache entry to a tempfile and then atomically
                // move it to its final location so that other rustc invocations
                // happening in parallel don't see a partially-written file.
                // Rustc can hard-link its outputs to an incremental session.
                // Replacing the destination also leaves that session intact.
                let mut tmp = match NamedTempFile::new_in(dir) {
                    Ok(tmp) => tmp,
                    Err(err) => {
                        if !optional || Self::object_index(&self.zip, &key).is_some() {
                            self.extract_object_to_new_file(&key, &path)
                                .with_context(|| {
                                    format!(
                                        "Failed to restore {} after tempfile error: {err}",
                                        path.display()
                                    )
                                })?;
                        }
                        continue;
                    }
                };
                match self.extract_object(&key, tmp.as_file_mut()) {
                    Ok(mode) => {
                        if let Some(mode) = mode {
                            set_file_mode(tmp.as_file(), mode)?;
                        }
                        tmp.persist(&path)?;
                    }
                    Err(_) if optional => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
        .await?
    }

    fn extract_object_to_new_file(&mut self, key: &str, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        // A concurrent writer may have recreated the name after unlink.
        let mut file = fs::File::create_new(path)?;
        if let Some(mode) = self.get_object(key, &mut file)? {
            set_file_mode(file.file(), mode)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn is_output_stream(path: &Path) -> bool {
    path == Path::new("/dev/stdout")
        || path == Path::new("/dev/stderr")
        || path.parent() == Some(Path::new("/dev/fd"))
}

#[cfg(unix)]
fn open_output_stream(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::FileTypeExt;

    // A descriptor path can point to a regular file. Check the open handle
    // before writing, without truncating anything on open.
    let file = fs::OpenOptions::new().write(true).open(path)?;
    let kind = file.metadata()?.file_type();
    if !kind.is_fifo() && !kind.is_char_device() {
        bail!(
            "Output stream {} refers to a regular output",
            path.display()
        );
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn reflink_object(
    source: &File,
    output: &File,
    offset: u64,
    head: u64,
    tail: u64,
) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    let tail_offset = offset.checked_add(head).ok_or(io::ErrorKind::InvalidData)?;
    clone_range(source, offset, output, 0, head)?;
    if tail > 0 {
        // The duplicate source fd shares the ZIP reader's seek position.
        let mut bytes = [0; RAW_ALIGNMENT as usize];
        let bytes = &mut bytes[..tail as usize];
        source.read_exact_at(bytes, tail_offset)?;
        output.write_all_at(bytes, head)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn clone_range(
    source: &File,
    src_offset: u64,
    output: &File,
    dest_offset: u64,
    length: u64,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let range = libc::file_clone_range {
        src_fd: source.as_raw_fd().into(),
        src_offset,
        src_length: length,
        dest_offset,
    };
    // SAFETY: FICLONERANGE reads the initialized range and both fds stay open.
    let result = unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONERANGE, &raw const range) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn log_reflink_failure(error: &io::Error) {
    static LOG_FAILURE: std::sync::Once = std::sync::Once::new();
    LOG_FAILURE.call_once(|| info!("Disk cache reflink failed, copying objects: {error}"));
}

#[cfg(unix)]
fn is_path_null(path: &Path) -> bool {
    path == Path::new("/dev/null")
}

#[cfg(windows)]
fn is_path_null(path: &Path) -> bool {
    // For Windows, it appears that `NUL` with whatever extension is also a blackhole
    // (at least for `CreateFileX`), so it does not suffice to check for an exact match
    // Also note that gcc, cl.exe, et al. append a correct extension automatically even
    // if the user asks for output to `NUL`.
    let Some(stem) = path.file_stem() else {
        return false;
    };
    stem.eq_ignore_ascii_case("NUL")
}

pub struct OpenCacheObject {
    key: String,
    file: fs::File,
}

/// Data to be stored in the compiler cache.
pub struct CacheWrite {
    zip: ZipWriter<Cursor<Vec<u8>>>,
    has_raw_objects: bool,
}

impl CacheWrite {
    /// Create a new, empty cache entry.
    pub fn new() -> CacheWrite {
        CacheWrite {
            zip: ZipWriter::new(Cursor::new(vec![])),
            has_raw_objects: false,
        }
    }

    /// Create a new cache entry populated with the contents of `objects`.
    pub async fn from_objects<T>(
        objects: T,
        pool: &tokio::runtime::Handle,
        raw: bool,
    ) -> Result<CacheWrite>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        let objects = pool
            .spawn_blocking(move || Self::open_objects(objects))
            .await??;
        Self::from_open_objects(objects, pool, raw).await
    }

    pub(crate) fn open_objects(
        objects: impl IntoIterator<Item = FileObjectSource>,
    ) -> Result<Vec<OpenCacheObject>> {
        let mut opened = Vec::new();
        for FileObjectSource {
            key,
            path,
            optional,
        } in objects
        {
            match fs::File::open(&path) {
                Ok(file) => opened.push(OpenCacheObject { key, file }),
                Err(_) if optional => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("failed to open file {path:?}"));
                }
            }
        }
        Ok(opened)
    }

    pub(crate) async fn from_open_objects(
        objects: Vec<OpenCacheObject>,
        pool: &tokio::runtime::Handle,
        raw: bool,
    ) -> Result<CacheWrite> {
        pool.spawn_blocking(move || Self::pack_objects(objects, raw))
            .await?
    }

    pub(super) fn pack_objects(objects: Vec<OpenCacheObject>, raw: bool) -> Result<Self> {
        let mut entry = Self::new();
        for OpenCacheObject { key, mut file } in objects {
            let mode = get_file_mode(&file)?;
            let result = if raw {
                let large = file.metadata()?.len() >= zip::ZIP64_BYTES_THR;
                entry.put_raw_object(&key, &mut file, mode, large)
            } else {
                entry.put_object(&key, &mut file, mode)
            };
            result.with_context(|| {
                format!("failed to put object {:?} in cache entry", file.path())
            })?;
        }
        Ok(entry)
    }

    /// Add an object containing the contents of `from` to this cache entry at `name`.
    /// If `mode` is `Some`, store the file entry with that mode.
    pub fn put_object<T>(&mut self, name: &str, from: &mut T, mode: Option<u32>) -> Result<()>
    where
        T: Read,
    {
        Self::put_object_into(&mut self.zip, name, from, mode)
    }

    fn put_object_into<W: Write + Seek, R: Read>(
        zip: &mut ZipWriter<W>,
        name: &str,
        from: &mut R,
        mode: Option<u32>,
    ) -> Result<()> {
        // We're going to declare the compression method as "stored",
        // but we're actually going to store zstd-compressed blobs.
        zip.start_file(name, Self::object_options(mode))
            .context("Failed to start cache entry object")?;

        let compression_level = std::env::var("SCCACHE_CACHE_ZSTD_LEVEL")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(3);
        zstd::stream::copy_encode(from, zip, compression_level)?;
        Ok(())
    }

    fn put_raw_object<T: Read>(
        &mut self,
        name: &str,
        from: &mut T,
        mode: Option<u32>,
        large: bool,
    ) -> Result<()> {
        Self::start_raw_object(&mut self.zip, name, mode, large)?;
        io::copy(from, &mut self.zip)?;
        self.has_raw_objects = true;
        Ok(())
    }

    fn start_raw_object<W: Write + Seek>(
        zip: &mut ZipWriter<W>,
        name: &str,
        mode: Option<u32>,
        large: bool,
    ) -> Result<()> {
        zip.start_file(
            format!("raw/{name}"),
            Self::object_options(mode)
                .with_alignment(RAW_ALIGNMENT)
                .large_file(large),
        )
        .context("Failed to start cache entry object")
    }

    pub(super) fn has_raw_objects(&self) -> bool {
        self.has_raw_objects
    }

    fn object_options(mode: Option<u32>) -> SimpleFileOptions {
        // zip 0.6 wrote entries with Unix creator metadata on every platform.
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .system(System::Unix);
        match mode {
            Some(mode) => opts.unix_permissions(mode),
            None => opts,
        }
    }

    pub fn put_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        self.put_bytes("stdout", bytes)
    }

    pub fn put_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        self.put_bytes("stderr", bytes)
    }

    fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        Self::put_bytes_into(&mut self.zip, name, bytes)
    }

    fn put_bytes_into<W: Write + Seek>(
        zip: &mut ZipWriter<W>,
        name: &str,
        mut bytes: &[u8],
    ) -> Result<()> {
        if !bytes.is_empty() {
            Self::put_object_into(zip, name, &mut bytes, None)?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn try_reflink_objects(
        mut output: &File,
        objects: &[OpenCacheObject],
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<bool> {
        use std::os::unix::fs::FileExt;

        let mut zip = ZipWriter::new(ReflinkZipFile {
            file: output,
            cloned_remaining: std::cell::Cell::new(0),
        });
        let alignment = u64::from(RAW_ALIGNMENT);
        let mut buf = vec![0; 128 * 1024];
        for OpenCacheObject { key, file } in objects {
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Ok(false);
            }
            let len = metadata.len();
            let mode = get_file_mode(file)?;
            Self::start_raw_object(&mut zip, key, mode, len >= zip::ZIP64_BYTES_THR)?;

            let offset = output.stream_position()?;
            let head = len - len % alignment;
            if head > 0 {
                if let Err(error) = clone_range(file.file(), 0, output, offset, head) {
                    debug!("Disk cache store reflink failed, copying objects: {error}");
                    return Ok(false);
                }
                // CRC the cloned bytes so a later write to the source can't
                // make the checksum disagree with the archive.
                zip.get_ref().unwrap().cloned_remaining.set(head);
                let mut copied = 0;
                while copied < head {
                    let size = (head - copied).min(buf.len() as u64) as usize;
                    let chunk = &mut buf[..size];
                    output.read_exact_at(chunk, offset + copied)?;
                    zip.write_all(chunk)?;
                    copied += size as u64;
                }
            }

            let tail = &mut buf[..(len - head) as usize];
            file.file().read_exact_at(tail, head)?;
            zip.write_all(tail)?;
        }
        Self::put_bytes_into(&mut zip, "stdout", stdout)?;
        Self::put_bytes_into(&mut zip, "stderr", stderr)?;
        zip.finish().context("Failed to finish cache entry zip")?;
        Ok(true)
    }

    /// Finish writing data to the cache entry writer, and return the data.
    pub fn finish(self) -> Result<Vec<u8>> {
        let cur = self
            .zip
            .finish()
            .context("Failed to finish cache entry zip")?;
        Ok(cur.into_inner())
    }
}

#[cfg(target_os = "linux")]
struct ReflinkZipFile<'a> {
    file: &'a File,
    cloned_remaining: std::cell::Cell<u64>,
}

#[cfg(target_os = "linux")]
impl Write for ReflinkZipFile<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.cloned_remaining.get();
        if remaining == 0 {
            return self.file.write(buf);
        }
        // ZipWriter still counts and checksums these bytes.
        let skip = remaining.min(buf.len() as u64);
        self.file.seek_relative(skip as i64)?;
        self.cloned_remaining.set(remaining - skip);
        Ok(skip as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(target_os = "linux")]
impl Seek for ReflinkZipFile<'_> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl Default for CacheWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_SIZES: [usize; 6] = [0, 1, 4095, 4096, 4097, 1024 * 1024 + 7];

    fn raw_member(size: usize) -> (String, Vec<u8>) {
        (format!("object-{size}"), vec![(size % 251) as u8; size])
    }

    fn raw_entry(sizes: &[usize]) -> CacheWrite {
        let mut entry = CacheWrite::new();
        for &size in sizes {
            let (key, payload) = raw_member(size);
            entry
                .put_raw_object(&key, &mut payload.as_slice(), Some(0o755), false)
                .unwrap();
        }
        entry
    }

    #[test]
    fn raw_cache_entry_alignment_and_roundtrip() {
        let bytes = raw_entry(&RAW_SIZES).finish().unwrap();
        let mut entry = CacheRead::from(Cursor::new(bytes)).unwrap();
        for size in RAW_SIZES {
            let (key, payload) = raw_member(size);
            let file = entry.zip.by_name(&format!("raw/{key}")).unwrap();
            assert_eq!(file.data_start().unwrap() % u64::from(RAW_ALIGNMENT), 0);
            assert_eq!(file.compression(), CompressionMethod::Stored);
            assert_eq!(file.size(), size as u64);
            drop(file);
            let mut output = Vec::new();
            let mode = entry.get_object(&key, &mut output).unwrap().unwrap();
            assert_eq!(mode & 0o777, 0o755);
            assert_eq!(output, payload);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn raw_cache_entry_reflink_store_matches_packed_bytes_and_is_copy_on_write() {
        use std::os::unix::fs::FileExt;

        let dir = tempfile::tempdir().unwrap();
        if let Err(error) = super::super::disk::probe_reflink(dir.path()) {
            eprintln!("skipping store-side reflink check: FICLONE failed: {error}");
            return;
        }
        let sources = RAW_SIZES.map(|size| {
            let (key, payload) = raw_member(size);
            let path = dir.path().join(&key);
            fs::write(&path, &payload).unwrap();
            FileObjectSource {
                key,
                path,
                optional: false,
            }
        });
        let mut packed =
            CacheWrite::pack_objects(CacheWrite::open_objects(sources.clone()).unwrap(), true)
                .unwrap();
        packed.put_stdout(b"compiler stdout").unwrap();
        packed.put_stderr(b"compiler stderr").unwrap();
        let expected = packed.finish().unwrap();
        let held = CacheWrite::open_objects(sources.clone()).unwrap();
        let large = sources.last().unwrap();
        let original = dir.path().join("original");
        fs::rename(&large.path, &original).unwrap();
        fs::write(&large.path, b"replacement").unwrap();

        let archive = NamedTempFile::new_in(dir.path()).unwrap();
        assert!(
            CacheWrite::try_reflink_objects(
                archive.as_file(),
                &held,
                b"compiler stdout",
                b"compiler stderr"
            )
            .unwrap()
        );
        assert_eq!(fs::read(archive.path()).unwrap(), expected);
        let source = fs::OpenOptions::new().write(true).open(&original).unwrap();
        assert_shared_extents(source.file(), 1024 * 1024);
        source.file().write_all_at(&[0x55; 4096], 0).unwrap();
        assert_eq!(fs::read(archive.path()).unwrap(), expected);
        eprintln!("verified byte-identical raw ZIPs and copy-on-write from the held inode");
    }

    #[test]
    fn raw_cache_entry_mixed_members() {
        let mut entry = raw_entry(&[4096, 4097]);
        entry
            .put_object("legacy", &mut &b"compressed"[..], None)
            .unwrap();
        entry
            .put_object("object-4096", &mut &b"preferred"[..], None)
            .unwrap();
        entry.put_stdout(b"stdout text").unwrap();
        entry.put_stderr(b"stderr text").unwrap();
        let mut entry = CacheRead::from(Cursor::new(entry.finish().unwrap())).unwrap();
        for (name, expected) in [
            ("legacy", b"compressed".as_slice()),
            ("object-4096", b"preferred"),
            ("object-4097", &raw_member(4097).1),
        ] {
            let mut output = Vec::new();
            entry.get_object(name, &mut output).unwrap();
            assert_eq!(output, expected);
        }
        assert_eq!(entry.get_stdout(), b"stdout text");
        assert_eq!(entry.get_stderr(), b"stderr text");
    }

    #[test]
    fn raw_cache_entry_legacy_lookup_is_decompression_failure() {
        let bytes = raw_entry(&[4096]).finish().unwrap();
        let mut entry = CacheRead::from(Cursor::new(bytes)).unwrap();
        // The old reader maps a missing unprefixed ZIP member to DecompressionFailure.
        let Err(error) = entry
            .zip
            .by_name("object-4096")
            .or(Err(DecompressionFailure))
        else {
            panic!("legacy reader found an unprefixed member");
        };
        assert!(anyhow!(error).is::<DecompressionFailure>());
        let mut output = Vec::new();
        entry.get_object("object-4096", &mut output).unwrap();
        assert_eq!(output, raw_member(4096).1);
    }

    #[cfg(unix)]
    #[test]
    fn raw_cache_entry_hit_replaces_inode() {
        use std::os::unix::fs::MetadataExt;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output");
        let session = dir.path().join("session");
        fs::write(&path, b"session metadata").unwrap();
        fs::hard_link(&path, &session).unwrap();
        let archive = dir.path().join("entry.zip");
        fs::write(&archive, raw_entry(&[4097]).finish().unwrap()).unwrap();
        let reader = CacheRead::from_file(fs::File::open(archive).unwrap(), true).unwrap();
        runtime
            .block_on(reader.extract_objects(
                [FileObjectSource {
                    key: raw_member(4097).0,
                    path: path.clone(),
                    optional: false,
                }],
                runtime.handle(),
            ))
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), raw_member(4097).1);
        assert_eq!(fs::read(&session).unwrap(), b"session metadata");
        assert_ne!(
            fs::metadata(&path).unwrap().ino(),
            fs::metadata(&session).unwrap().ino()
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn raw_cache_entry_clone_failure_copies_members() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("entry.zip");
        let sizes = [4097, 8193];
        fs::write(&archive, raw_entry(&sizes).finish().unwrap()).unwrap();
        let mut reader = CacheRead::from_file(fs::File::open(&archive).unwrap(), true).unwrap();
        if super::super::disk::probe_reflink(dir.path()).is_ok() {
            // A write-only source forces the clone to fail even on a reflink-capable FS.
            reader.reflink_source = Some(File::options().write(true).open(&archive).unwrap());
        }
        for size in sizes {
            let (key, payload) = raw_member(size);
            let mut output = NamedTempFile::new_in(dir.path()).unwrap();
            assert_eq!(
                reader
                    .extract_object(&key, output.as_file_mut())
                    .unwrap()
                    .unwrap()
                    & 0o777,
                0o755,
            );
            assert!(reader.reflink_source.is_none());
            assert_eq!(fs::read(output.path()).unwrap(), payload);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn raw_cache_entry_disk_hit_shares_aligned_extents() {
        use crate::cache::{CacheStorage, disk::DiskCache};

        let dir = tempfile::tempdir().unwrap();
        if let Err(error) = super::super::disk::probe_reflink(dir.path()) {
            eprintln!("skipping shared extent check: FICLONE failed: {error}");
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cache = DiskCache::new(
            dir.path().join("cache"),
            10 * 1024 * 1024,
            runtime.handle(),
            Default::default(),
            CacheMode::ReadWrite,
            vec![],
        )
        .with_reflink(true);
        assert!(cache.write_raw_objects());
        let key = "0123456789abcdef";
        runtime.block_on(async {
            cache.put(key, raw_entry(&RAW_SIZES)).await.unwrap();
            let Cache::Hit(entry) = cache.get(key).await.unwrap() else {
                panic!("expected disk cache hit");
            };
            entry
                .extract_objects(
                    RAW_SIZES.map(|size| FileObjectSource {
                        key: raw_member(size).0,
                        path: dir.path().join(size.to_string()),
                        optional: false,
                    }),
                    runtime.handle(),
                )
                .await
                .unwrap();
        });
        for size in RAW_SIZES {
            let path = dir.path().join(size.to_string());
            assert_eq!(fs::read(&path).unwrap(), raw_member(size).1);
            let head = size & !(usize::from(RAW_ALIGNMENT) - 1);
            if head != 0 {
                assert_shared_extents(&File::open(&path).unwrap(), head as u64);
            }
        }
        eprintln!("verified FIEMAP_EXTENT_SHARED and output bytes for {RAW_SIZES:?}");
    }

    #[test]
    fn raw_cache_entry_without_compression_counts_full_length() {
        use crate::cache::{CacheStorage, disk::DiskCache};

        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cache = DiskCache::new(
            dir.path(),
            10 * 1024 * 1024,
            runtime.handle(),
            Default::default(),
            CacheMode::ReadWrite,
            vec![],
        );
        let size = 1024 * 1024;
        let length = raw_entry(&[size]).finish().unwrap().len() as u64;
        runtime.block_on(async {
            cache.check().await.unwrap();
            cache
                .put("0123456789abcdef", raw_entry(&[size]))
                .await
                .unwrap();
            assert_eq!(cache.current_size().await.unwrap(), Some(length));
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn raw_cache_entry_btrfs_weighted_eviction_keeps_reflink_output() {
        use crate::cache::{CacheStorage, disk::DiskCache};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        fs::create_dir(&root).unwrap();
        if !crate::lru_disk_cache::test_btrfs_zstd(&root) {
            return;
        }
        if let Err(error) = super::super::disk::probe_reflink(&root) {
            eprintln!("skipping reflink eviction check: FICLONE failed: {error}");
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let capacity = 64 * 1024;
        let cache = DiskCache::new(
            &root,
            capacity,
            runtime.handle(),
            Default::default(),
            CacheMode::ReadWrite,
            vec![],
        )
        .with_reflink(true);
        let size = 1024 * 1024 + 7;
        let (object, payload) = raw_member(size);
        let output = dir.path().join("object");
        let key = "0123456789abcdef";
        runtime.block_on(async {
            cache.check().await.unwrap();
            cache.put(key, raw_entry(&[size])).await.unwrap();
            let Cache::Hit(entry) = cache.get(key).await.unwrap() else {
                panic!("expected disk cache hit");
            };
            entry
                .extract_objects(
                    [FileObjectSource {
                        key: object,
                        path: output.clone(),
                        optional: false,
                    }],
                    runtime.handle(),
                )
                .await
                .unwrap();
            assert_shared_extents(&File::open(&output).unwrap(), 1024 * 1024);
            cache
                .put("1123456789abcdef", raw_entry(&[size]))
                .await
                .unwrap();
            assert!(matches!(cache.get(key).await.unwrap(), Cache::Miss));
            assert!(!root.join("0/1").join(key).exists());
            assert_eq!(fs::read(&output).unwrap(), payload);
        });
        eprintln!(
            "verified shared extents before eviction and {size} intact output bytes after eviction"
        );
    }

    #[cfg(target_os = "linux")]
    fn assert_shared_extents(file: &File, length: u64) {
        use std::os::fd::AsRawFd;

        #[repr(C)]
        struct Fiemap {
            start: u64,
            length: u64,
            flags: u32,
            mapped_extents: u32,
            extent_count: u32,
            reserved: u32,
        }

        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Extent {
            logical: u64,
            physical: u64,
            length: u64,
            reserved64: [u64; 2],
            flags: u32,
            reserved: [u32; 3],
        }

        #[repr(C)]
        struct FiemapRequest {
            header: Fiemap,
            extents: [Extent; 256],
        }

        const FIEMAP_FLAG_SYNC: u32 = 1;
        const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
        let mut request = FiemapRequest {
            header: Fiemap {
                start: 0,
                length,
                flags: FIEMAP_FLAG_SYNC,
                mapped_extents: 0,
                extent_count: 256,
                reserved: 0,
            },
            extents: [Extent::default(); 256],
        };
        // SAFETY: The request has the Linux FIEMAP layout and room for extent_count results.
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                libc::_IOWR::<Fiemap>(u32::from(b'f'), 11),
                &raw mut request,
            )
        };
        assert_eq!(result, 0, "FIEMAP: {}", io::Error::last_os_error());
        let mut end = 0;
        for extent in &request.extents[..request.header.mapped_extents as usize] {
            assert_eq!(extent.logical, end);
            assert_ne!(
                extent.flags & FIEMAP_EXTENT_SHARED,
                0,
                "unshared extent at {end}"
            );
            end += extent.length;
        }
        assert!(end >= length, "shared only {end} of {length} bytes");
    }

    #[test_case::test_case(false ; "compressed")]
    #[test_case::test_case(true ; "raw")]
    fn cache_entry_keeps_the_open_inode(raw: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output");
        let replacement = dir.path().join("replacement");
        fs::write(&path, b"original output").unwrap();
        let opened = CacheWrite::open_objects([
            FileObjectSource {
                key: "output".into(),
                path: path.clone(),
                optional: false,
            },
            FileObjectSource {
                key: "optional".into(),
                path: dir.path().join("absent"),
                optional: true,
            },
        ])
        .unwrap();

        fs::write(&replacement, b"replacement output").unwrap();
        fs::rename(&replacement, &path).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let entry = runtime
            .block_on(CacheWrite::from_open_objects(opened, runtime.handle(), raw))
            .unwrap();
        let mut entry = CacheRead::from(Cursor::new(entry.finish().unwrap())).unwrap();
        let mut output = Vec::new();
        entry.get_object("output", &mut output).unwrap();
        assert_eq!(output, b"original output");
        assert!(entry.get_object("optional", &mut Vec::new()).is_err());
    }

    #[test]
    fn cache_entry_roundtrip_preserves_zip_format() {
        let payload = b"cache payload";
        let mut source = Cursor::new(payload);
        let mut cache_write = CacheWrite::new();
        cache_write
            .put_object("object", &mut source, Some(0o755))
            .unwrap();
        let cache_bytes = cache_write.finish().unwrap();
        assert_eq!(&cache_bytes[..4], b"PK\x03\x04");

        let mut cache_read = CacheRead::from(Cursor::new(cache_bytes)).unwrap();
        let mut output = Vec::new();
        let mode = cache_read
            .get_object("object", &mut output)
            .unwrap()
            .unwrap();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(output, payload);
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_devnull_works() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let cache_data = CacheWrite::new();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: PathBuf::from("/dev/null"),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(result.is_ok(), "Extracting to /dev/null should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_dev_fd_pipe() {
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncReadExt;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (sender, mut reader) = tokio::net::unix::pipe::pipe().unwrap();
            let sender = sender.into_blocking_fd().unwrap();
            let path = PathBuf::from(format!("/dev/fd/{}", sender.as_raw_fd()));
            // FreeBSD may run without fdescfs mounted.
            if !path.exists() {
                return;
            }

            let mut cache_data = CacheWrite::new();
            cache_data.put_bytes("object", b"cache data").unwrap();
            let cache_read = CacheRead::from(Cursor::new(cache_data.finish().unwrap())).unwrap();
            let objects = vec![FileObjectSource {
                key: "object".into(),
                path,
                optional: false,
            }];
            cache_read
                .extract_objects(objects, runtime.handle())
                .await
                .unwrap();
            drop(sender);

            let mut output = Vec::new();
            reader.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, b"cache data");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_dev_fd_requires_atomic_restore() {
        use std::os::fd::AsRawFd;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let destination = tempfile::tempfile().unwrap();
        let path = PathBuf::from(format!("/dev/fd/{}", destination.as_raw_fd()));
        if !path.exists() {
            return;
        }

        let mut cache_data = CacheWrite::new();
        cache_data.put_bytes("object", b"cache data").unwrap();
        let cache_read = CacheRead::from(Cursor::new(cache_data.finish().unwrap())).unwrap();
        let objects = vec![FileObjectSource {
            key: "object".into(),
            path,
            optional: false,
        }];
        runtime
            .block_on(cache_read.extract_objects(objects, runtime.handle()))
            .unwrap_err();
        assert_eq!(destination.metadata().unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test_case::test_case(false, false ; "writable directory")]
    #[test_case::test_case(true, false ; "unwritable directory")]
    #[test_case::test_case(true, true ; "unwritable directory with optional raw object")]
    fn test_extract_object_preserves_existing_hard_link(unwritable: bool, raw: bool) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let tmpdir = tempfile::tempdir().unwrap();
        let out_dir = tmpdir.path().join("out");
        fs::create_dir(&out_dir).unwrap();
        let output = out_dir.join("libfoo.rmeta");
        let session = tmpdir.path().join("metadata.rmeta");
        fs::write(&output, b"session data").unwrap();
        fs::hard_link(&output, &session).unwrap();
        let old_inode = fs::metadata(&session).unwrap().ino();

        let mut cache_data = CacheWrite::new();
        if raw {
            cache_data
                .put_raw_object("libfoo.rmeta", &mut &b"cache data"[..], None, false)
                .unwrap();
        } else {
            cache_data.put_bytes("libfoo.rmeta", b"cache data").unwrap();
        }
        let cache_read = CacheRead::from(Cursor::new(cache_data.finish().unwrap())).unwrap();
        let objects = vec![FileObjectSource {
            key: "libfoo.rmeta".into(),
            path: output.clone(),
            optional: raw,
        }];

        if unwritable {
            let uid = std::process::Command::new("id").arg("-u").output().unwrap();
            assert!(uid.status.success());
            // Root can modify a directory with mode 0555.
            if uid.stdout == b"0\n" {
                eprintln!("Skipping unwritable-directory permissions check as root");
                return;
            }
            fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let result = runtime.block_on(cache_read.extract_objects(objects, runtime.handle()));
        if unwritable {
            fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            result.unwrap_err();
            assert_eq!(fs::read(&output).unwrap(), b"session data");
            assert_eq!(fs::metadata(&output).unwrap().ino(), old_inode);
        } else {
            result.unwrap();
            assert_eq!(fs::read(&output).unwrap(), b"cache data");
            assert_ne!(fs::metadata(&output).unwrap().ino(), old_inode);
        }
        assert_eq!(fs::read(&session).unwrap(), b"session data");
        assert_eq!(fs::metadata(&session).unwrap().ino(), old_inode);
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_without_tempfile_creates_fresh_inode() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let output = tmp.path().join("output");
        let link = tmp.path().join("linked-output");
        fs::write(&output, b"old data").unwrap();
        fs::hard_link(&output, &link).unwrap();
        let inode = fs::metadata(&link).unwrap().ino();

        let mut cache_data = CacheWrite::new();
        cache_data.put_bytes("object", b"cache data").unwrap();
        let mut cache_read = CacheRead::from(Cursor::new(cache_data.finish().unwrap())).unwrap();
        cache_read
            .extract_object_to_new_file("object", &output)
            .unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"cache data");
        assert_ne!(fs::metadata(&output).unwrap().ino(), inode);
        assert_eq!(fs::read(&link).unwrap(), b"old data");

        let dir = tmp.path().join("directory");
        fs::create_dir(&dir).unwrap();
        cache_read
            .extract_object_to_new_file("object", &dir)
            .unwrap_err();
        assert!(dir.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn test_extract_object_to_nul_works() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let cache_data = CacheWrite::new();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: PathBuf::from("NUL"),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(result.is_ok(), "Extracting to NUL should succeed");
    }
}
