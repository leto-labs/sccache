use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

pub fn clock() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
}

pub fn day(seconds: u64) -> String {
    let seconds = seconds as libc::time_t;
    // SAFETY: gmtime_r takes initialized time and writes a tm to our buffer.
    let mut utc: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&seconds, &mut utc) };
    format!(
        "{:04}-{:02}-{:02}",
        utc.tm_year + 1900,
        utc.tm_mon + 1,
        utc.tm_mday
    )
}

pub fn root() -> io::Result<PathBuf> {
    root_for("v0")
}

pub fn root_for(version: &str) -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("LETO_TEST_CACHE_DIR") {
        return Ok(path.into());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unset"))?;
    Ok(PathBuf::from(home)
        .join(".cache/leto-test-results")
        .join(version))
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    pub mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    pub fn from_meta(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::from_meta(&fs::metadata(path)?))
    }

    pub fn id(self) -> String {
        format!(
            "{:x}-{:x}-{:x}-{:x}-{:x}-{:x}-{:x}",
            self.dev, self.ino, self.size, self.mtime.0, self.mtime.1, self.ctime.0, self.ctime.1
        )
    }

    fn recent(self, now: Duration) -> bool {
        let threshold = now.saturating_sub(Duration::from_secs(2));
        let cutoff = (
            threshold.as_secs() as i64,
            i64::from(threshold.subsec_nanos()),
        );
        self.mtime >= cutoff || self.ctime >= cutoff
    }
}

pub fn digest(
    root: &Path,
    path: &Path,
    stamp: Stamp,
    now: Duration,
    write_memo: bool,
) -> io::Result<blake3::Hash> {
    let memo = root.join("digests").join(stamp.id());
    let memoize = !stamp.recent(now);
    if memoize
        && let Ok(value) = fs::read_to_string(&memo)
        && let Ok(hash) = blake3::Hash::from_hex(&value)
        && Stamp::read(path)? == stamp
    {
        return Ok(hash);
    }

    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || Stamp::from_meta(&metadata) != stamp {
        return Err(io::Error::other("digest input changed before reading"));
    }
    let mut hash = blake3::Hasher::new();
    hash.update_reader(&mut file)?;
    if Stamp::from_meta(&file.metadata()?) != stamp || Stamp::read(path)? != stamp {
        return Err(io::Error::other("digest input changed while reading"));
    }
    let hash = hash.finalize();
    if memoize && write_memo {
        replace(&memo, hash.to_hex().as_bytes())?;
    }
    Ok(hash)
}

pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static SEQUENCE: AtomicU32 = AtomicU32::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent"))?;
    fs::create_dir_all(parent)?;
    let mut name = OsString::from(".tmp-");
    name.push(format!(
        "{}-{}-{}",
        std::process::id(),
        clock().as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = parent.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn log_file(root: &Path, kind: &str, utc_day: &str) -> io::Result<File> {
    let dir = root.join(kind);
    fs::create_dir_all(&dir)?;
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(dir.join(format!("{utc_day}.jsonl")))
}

pub fn append(mut file: &File, value: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    if line.len() >= 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized shadow record",
        ));
    }
    line.push(b'\n');
    loop {
        match file.write(&line) {
            Ok(n) if n == line.len() => return Ok(()),
            Ok(_) => return Err(io::Error::other("short shadow log write")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

pub fn error(root: Option<&Path>, binary: &Path, error: &io::Error) {
    let now = clock();
    let fallback = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/leto-test-results/v0"));
    let utc_day = day(now.as_secs());
    let record = json!({
        "time_ns": now.as_nanos() as u64,
        "binary": clipped(&binary.to_string_lossy(), 480),
        "error": clipped(&error.to_string(), 480),
    });
    for path in root.into_iter().chain(fallback.as_deref()) {
        if let Ok(file) = log_file(path, "errors", &utc_day)
            && append(&file, &record).is_ok()
        {
            return;
        }
    }
}

pub fn clipped(value: &str, limit: usize) -> &str {
    &value[..value.floor_char_boundary(limit.min(value.len()))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_memo_skips_recent_files_and_invalidates_rewrites() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "leto-digest-{}-{}",
            std::process::id(),
            clock().as_nanos()
        ));
        fs::create_dir(&root)?;
        let file = root.join("bin");
        fs::write(&file, "abc")?;
        let stamp = Stamp::read(&file)?;
        let now = clock();
        let first = digest(&root, &file, stamp, now, true)?;
        let memo = root.join("digests").join(stamp.id());
        assert!(!memo.exists());
        let later = now + Duration::from_secs(3);
        assert_eq!(digest(&root, &file, stamp, later, true)?, first);
        assert!(memo.exists());

        fs::write(&file, "def")?;
        let changed = Stamp::read(&file)?;
        assert_eq!(stamp.size, changed.size);
        assert_ne!(stamp, changed);
        assert!(digest(&root, &file, stamp, clock(), true).is_err());
        assert_ne!(digest(&root, &file, changed, clock(), true)?, first);
        assert!(!root.join("digests").join(changed.id()).exists());
        fs::remove_dir_all(root)
    }

    #[test]
    fn log_serializes_one_json_line() -> io::Result<()> {
        let file = std::env::temp_dir().join(format!(
            "leto-log-{}-{}",
            std::process::id(),
            clock().as_nanos()
        ));
        let data = json!({"key": "a", "test": "a\nb", "outcome": { "signal": 15 }});
        append(
            &OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(&file)?,
            &data,
        )?;
        let raw = fs::read_to_string(&file)?;
        fs::remove_file(file)?;
        assert_eq!(raw.lines().count(), 1);
        assert_eq!(serde_json::from_str::<Value>(&raw)?, data);
        Ok(())
    }
}
