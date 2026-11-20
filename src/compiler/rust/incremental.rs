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

use super::ParsedArguments;
use filetime::FileTime;
use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

const ONE_HOUR: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug)]
pub(crate) struct IncrementalSession {
    path: PathBuf,
    crate_name: String,
}

impl IncrementalSession {
    pub(super) fn from_parsed(args: &ParsedArguments) -> Option<Self> {
        let dir = args.incremental_dir.as_deref()?;
        let hash = args.unit_hash(b"rust-incremental-argv");
        Some(Self {
            path: dir.join(format!("{}-sccache-{}", args.crate_name, &hash[..16])),
            crate_name: args.crate_name.clone(),
        })
    }

    pub(crate) fn refresh_mtime(&self) {
        // Cleanup tools use the mtime of the top-level incremental directory entry.
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.is_dir()
        {
            let _ = filetime::set_symlink_file_times(
                &self.path,
                FileTime::from_last_access_time(&metadata),
                FileTime::now(),
            );
        }
    }

    pub(crate) fn prune(&self, days: u64) {
        if days != 0
            && let Err(error) = self.prune_siblings(Duration::from_secs(days.saturating_mul(86400)))
        {
            debug!(
                "Could not prune incremental siblings of {:?}: {error}",
                self.path
            );
        }
    }

    fn prune_siblings(&self, max_age: Duration) -> io::Result<()> {
        let root = self.path.parent().unwrap();
        // Check from the outside in so a symlink in the path cannot redirect the sweep.
        let ancestors: Vec<_> = root.ancestors().collect();
        for dir in ancestors.into_iter().rev() {
            if !dir.as_os_str().is_empty() && !fs::symlink_metadata(dir)?.is_dir() {
                return Ok(());
            }
        }

        let now = SystemTime::now();
        let marker = root.join(format!("{}-sccache-prune", self.crate_name));
        if let Ok(metadata) = fs::symlink_metadata(&marker)
            && (!metadata.is_file() || (metadata.len() != 0 && !is_older(&metadata, now, ONE_HOUR)))
        {
            return Ok(());
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(
            &mut options,
            libc::O_NOFOLLOW | libc::O_NONBLOCK,
        );
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::custom_flags(
            &mut options,
            windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
        );
        let mut marker = options.open(marker)?;
        if marker.try_lock().is_err() {
            return Ok(());
        }
        let metadata = marker.metadata()?;
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
            return Ok(());
        }
        if !metadata.is_file() || (metadata.len() != 0 && !is_older(&metadata, now, ONE_HOUR)) {
            return Ok(());
        }
        // Multiple callers can open a new marker before any of them acquires the lock.
        if metadata.len() == 0 {
            marker.write_all(b"1")?;
        }
        marker.set_modified(now)?;

        let prefix = format!("{}-", self.crate_name);
        for entry in fs::read_dir(root)?.flatten() {
            let name = entry.file_name();
            let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
                continue;
            };
            let candidate = entry.path();
            if candidate != self.path
                && is_session_suffix(suffix)
                && is_stale_dir(&candidate, now, max_age)
                && has_no_recent_locks(&candidate, now)
                && is_stale_dir(&candidate, now, max_age)
                && let Err(error) = fs::remove_dir_all(&candidate)
            {
                debug!("Could not prune incremental directory {candidate:?}: {error}");
            }
        }
        Ok(())
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn has_finalized_session(&self) -> io::Result<bool> {
        // Cargo serializes builds in a build directory and schedules each unit once per build.
        // Rustc stores sessions one level below its crate directory.
        for entry in WalkDir::new(&self.path).max_depth(2) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err)
                    if err.depth() == 0
                        && err
                            .io_error()
                            .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    return Ok(false);
                }
                Err(err) => return Err(err.into()),
            };
            if entry.depth() == 0 {
                if !entry.file_type().is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "invalid incremental directory",
                    ));
                }
                continue;
            }
            let name = entry.file_name().as_encoded_bytes();
            if name.starts_with(b"s-") && !name.ends_with(b"-working") && !name.ends_with(b".lock")
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn is_session_suffix(suffix: &str) -> bool {
    if let Some(hash) = suffix.strip_prefix("sccache-") {
        hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    } else {
        // rustc encodes the stable crate ID in lowercase base 36.
        suffix.len() == 13
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
            && u64::from_str_radix(suffix, 36).is_ok()
    }
}

fn is_older(metadata: &Metadata, now: SystemTime, age: Duration) -> bool {
    metadata
        .modified()
        .is_ok_and(|mtime| now.duration_since(mtime).is_ok_and(|elapsed| elapsed > age))
}

fn is_stale_dir(path: &Path, now: SystemTime, age: Duration) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && is_older(&metadata, now, age))
}

fn has_no_recent_locks(path: &Path, now: SystemTime) -> bool {
    WalkDir::new(path)
        .follow_root_links(false)
        .into_iter()
        .all(|entry| {
            entry.is_ok_and(|entry| {
                !entry.file_type().is_file()
                    || !entry.file_name().as_encoded_bytes().ends_with(b".lock")
                    || entry
                        .metadata()
                        .is_ok_and(|metadata| is_older(&metadata, now, ONE_HOUR))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(root: &Path, suffix: &str) -> IncrementalSession {
        IncrementalSession {
            path: root.join(format!("mylib-sccache-{suffix}")),
            crate_name: "mylib".to_owned(),
        }
    }

    fn backdate(path: &Path, days: u64) {
        let old = SystemTime::now() - Duration::from_secs(days * 86400);
        filetime::set_file_mtime(path, FileTime::from_system_time(old)).unwrap();
    }

    fn fake_session(root: &Path, name: &str, days: u64) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        backdate(&dir, days);
        dir
    }

    #[test]
    fn incremental_pruning_removes_stale_private_and_flat_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let current = session(&root, "0000000000000000");
        fs::create_dir(current.path()).unwrap();
        backdate(current.path(), 8);
        let stale = fake_session(&root, "mylib-sccache-0000000000000001", 8);
        let flat = fake_session(&root, "mylib-01abc3ef4567z", 8);
        let recent = fake_session(&root, "mylib-sccache-0000000000000002", 1);
        let other = fake_session(&root, "mylib_extra-sccache-0000000000000003", 8);
        let invalid = fake_session(&root, "mylib-sccache-backup", 8);
        let locked = fake_session(&root, "mylib-sccache-0000000000000004", 8);
        let nested = locked.join("mylib-01abc3ef4567z");
        fs::create_dir(&nested).unwrap();
        let lock = nested.join("s-recent.lock");
        fs::write(&lock, "").unwrap();
        backdate(&locked, 8);
        let old_lock = flat.join("s-old.lock");
        fs::write(&old_lock, "").unwrap();
        backdate(&old_lock, 1);
        backdate(&flat, 8);

        current.prune(7);
        assert!(!stale.exists());
        assert!(!flat.exists());
        for kept in [current.path(), &recent, &other, &invalid, &locked] {
            assert!(kept.is_dir(), "removed {}", kept.display());
        }
    }

    #[test]
    fn incremental_pruning_is_disabled_at_zero_and_runs_at_most_hourly() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let a = session(&root, "000000000000000a");
        let b = session(&root, "000000000000000b");
        let stale = fake_session(&root, "mylib-sccache-0123456789abcdef", 8);
        let marker = root.join("mylib-sccache-prune");
        a.prune(0);
        assert!(stale.is_dir());
        assert!(!marker.exists());
        a.prune(7);
        assert!(!stale.exists());

        let stale = fake_session(&root, "mylib-sccache-0123456789abcdef", 8);
        b.prune(7);
        assert!(stale.is_dir());
        backdate(&marker, 1);
        b.prune(7);
        assert!(!stale.exists());
    }

    #[cfg(unix)]
    #[test]
    fn incremental_pruning_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = fake_session(&root, "outside", 8);
        let incremental = fake_session(&root, "incremental", 0);
        let stale = fake_session(&incremental, "mylib-sccache-0123456789abcdef", 8);
        symlink(&outside, stale.join("nested")).unwrap();
        backdate(&stale, 8);
        let link = incremental.join("mylib-sccache-0123456789abcde0");
        symlink(&outside, &link).unwrap();

        let linked_root = root.join("linked-root");
        symlink(&incremental, &linked_root).unwrap();
        session(&linked_root, "0000000000000000").prune(7);
        assert!(stale.is_dir());
        assert!(!incremental.join("mylib-sccache-prune").exists());

        session(&incremental, "0000000000000000").prune(7);
        assert!(!stale.exists());
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert!(outside.is_dir());
    }
}
