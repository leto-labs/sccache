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
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zip::read::{ArchiveOffset, Config as ZipReadConfig};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, System, ZipArchive, ZipWriter};

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
        Ok(CacheRead { zip: z })
    }

    /// Get an object from this cache entry at `name` and write it to `to`.
    /// If the file has stored permissions, return them.
    pub fn get_object<T>(&mut self, name: &str, to: &mut T) -> Result<Option<u32>>
    where
        T: Write,
    {
        let file = self.zip.by_name(name).or(Err(DecompressionFailure))?;
        if file.compression() != CompressionMethod::Stored {
            bail!(DecompressionFailure);
        }
        let mode = file.unix_mode();
        zstd::stream::copy_decode(file, to).or(Err(DecompressionFailure))?;
        Ok(mode)
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
                let mut tmp = NamedTempFile::new_in(dir)?;
                match self.get_object(&key, &mut tmp) {
                    Ok(mode) => {
                        tmp.persist(&path)?;
                        if let Some(mode) = mode {
                            set_file_mode(path.as_path(), mode)?;
                        }
                    }
                    Err(_) if optional => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
        .await?
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

/// Data to be stored in the compiler cache.
pub struct CacheWrite {
    zip: ZipWriter<Cursor<Vec<u8>>>,
}

impl CacheWrite {
    /// Create a new, empty cache entry.
    pub fn new() -> CacheWrite {
        CacheWrite {
            zip: ZipWriter::new(Cursor::new(vec![])),
        }
    }

    /// Create a new cache entry populated with the contents of `objects`.
    pub async fn from_objects<T>(objects: T, pool: &tokio::runtime::Handle) -> Result<CacheWrite>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
            let mut entry = CacheWrite::new();
            for FileObjectSource {
                key,
                path,
                optional,
            } in objects
            {
                let f = fs::File::open(&path)
                    .with_context(|| format!("failed to open file `{:?}`", path));
                match (f, optional) {
                    (Ok(mut f), _) => {
                        let mode = get_file_mode(&f)?;
                        entry.put_object(&key, &mut f, mode).with_context(|| {
                            format!("failed to put object `{:?}` in cache entry", path)
                        })?;
                    }
                    (Err(e), false) => return Err(e),
                    (Err(_), true) => continue,
                }
            }
            Ok(entry)
        })
        .await?
    }

    /// Add an object containing the contents of `from` to this cache entry at `name`.
    /// If `mode` is `Some`, store the file entry with that mode.
    pub fn put_object<T>(&mut self, name: &str, from: &mut T, mode: Option<u32>) -> Result<()>
    where
        T: Read,
    {
        // We're going to declare the compression method as "stored",
        // but we're actually going to store zstd-compressed blobs.
        // zip 0.6 wrote entries with Unix creator metadata on every platform.
        // Preserve that cache format explicitly with zip 9.
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .system(System::Unix);
        let opts = if let Some(mode) = mode {
            opts.unix_permissions(mode)
        } else {
            opts
        };
        self.zip
            .start_file(name, opts)
            .context("Failed to start cache entry object")?;

        let compression_level = std::env::var("SCCACHE_CACHE_ZSTD_LEVEL")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(3);
        zstd::stream::copy_encode(from, &mut self.zip, compression_level)?;
        Ok(())
    }

    pub fn put_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        self.put_bytes("stdout", bytes)
    }

    pub fn put_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        self.put_bytes("stderr", bytes)
    }

    fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        if !bytes.is_empty() {
            let mut cursor = Cursor::new(bytes);
            return self.put_object(name, &mut cursor, None);
        }
        Ok(())
    }

    /// Finish writing data to the cache entry writer, and return the data.
    pub fn finish(self) -> Result<Vec<u8>> {
        let CacheWrite { zip } = self;
        let cur = zip.finish().context("Failed to finish cache entry zip")?;
        Ok(cur.into_inner())
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
    #[test_case::test_case(false ; "writable directory")]
    #[test_case::test_case(true ; "unwritable directory")]
    fn test_extract_object_preserves_existing_hard_link(unwritable: bool) {
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
        cache_data.put_bytes("libfoo.rmeta", b"cache data").unwrap();
        let cache_read = CacheRead::from(Cursor::new(cache_data.finish().unwrap())).unwrap();
        let objects = vec![FileObjectSource {
            key: "libfoo.rmeta".into(),
            path: output.clone(),
            optional: false,
        }];

        if unwritable {
            fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            if NamedTempFile::new_in(&out_dir).is_ok() {
                fs::set_permissions(&out_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
                eprintln!(
                    "Skipping unwritable directory case: process can create a file in mode 0555"
                );
                return;
            }
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
