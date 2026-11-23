use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, OsRng, Payload, rand_core::RngCore},
};
use serde_json::{Value, json};

pub const OUTPUT_LIMIT: usize = 1 << 20;
const ENTRY_LIMIT: usize = OUTPUT_LIMIT + 4096;
const SHARD_LIMIT: u64 = (1 << 30) / 256;
const EXPIRY: Duration = Duration::from_secs(7 * 86_400);
const QUARANTINE: Duration = Duration::from_secs(30 * 86_400);
const MIN_VERIFIED: u64 = 20;

pub type Streams = (Vec<u8>, Vec<u8>);

pub enum Entry {
    Pass(Streams),
    Impure,
}

pub struct Store<'a> {
    pub root: &'a Path,
    pub key: blake3::Hash,
    pub identity: blake3::Hash,
}

pub fn test_identity(project: &OsStr, binary: &OsStr, test: &OsStr) -> blake3::Hash {
    let mut hash = blake3::Hasher::new_derive_key("leto test cache quarantine identity v1");
    for part in [project, binary, test] {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize()
}

fn recent(time: SystemTime, limit: Duration) -> bool {
    time.elapsed().is_ok_and(|age| age < limit)
}

fn active(path: &Path, limit: Duration) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(meta) => Ok(meta.modified()?.elapsed().map_or(true, |age| age < limit)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn lock(path: &Path) -> io::Result<File> {
    fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

impl Store<'_> {
    fn path(&self) -> PathBuf {
        let hex = self.key.to_hex();
        self.root.join(&hex[..2]).join(hex.as_str())
    }

    fn quarantine_path(&self) -> PathBuf {
        self.root
            .join("quarantine")
            .join(self.identity.to_hex().as_str())
    }

    pub fn quarantined(&self) -> io::Result<bool> {
        active(&self.quarantine_path(), QUARANTINE)
    }

    fn run_dir(&self, run_id: &OsStr) -> PathBuf {
        self.root
            .join("verification")
            .join(blake3::hash(run_id.as_bytes()).to_hex().as_str())
    }

    fn run_failure(&self, run_id: &OsStr) -> PathBuf {
        self.run_dir(run_id).join(self.identity.to_hex().as_str())
    }

    pub fn failed_verification(&self, run_id: &OsStr) -> io::Result<bool> {
        active(&self.run_failure(run_id), EXPIRY)
    }

    pub fn quarantine(&self, run_id: &OsStr) -> io::Result<()> {
        crate::store::replace(&self.run_failure(run_id), b"verification mismatch\n")?;
        crate::store::replace(&self.quarantine_path(), b"verification mismatch\n")?;
        let path = self.path();
        let _guard = lock(&path.with_file_name(".lock"))?;
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    pub fn lookup(&self, output_key: &[u8; 32]) -> io::Result<Option<Entry>> {
        if self.quarantined()? {
            return Ok(None);
        }
        let file = match File::open(self.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > ENTRY_LIMIT as u64 || !recent(meta.modified()?, EXPIRY) {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take(ENTRY_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
        Ok(self.decode(&bytes, output_key))
    }

    fn decode(&self, bytes: &[u8], output_key: &[u8; 32]) -> Option<Entry> {
        let newline = bytes.iter().take(4096).position(|&byte| byte == b'\n')?;
        let (header, payload) = bytes.split_at(newline + 1);
        let metadata: Value = serde_json::from_slice(header).ok()?;
        if metadata["mode"] != "gate"
            || metadata["version"] != 1
            || metadata["identity"] != self.identity.to_hex().as_str()
            || metadata["key"] != self.key.to_hex().as_str()
            || metadata["stored_ns"].as_u64().is_none_or(|stored| {
                let age = crate::store::clock().checked_sub(Duration::from_nanos(stored));
                age.is_none_or(|age| age >= EXPIRY)
            })
        {
            return None;
        }
        match metadata["status"].as_str()? {
            "impure" if payload.is_empty() => Some(Entry::Impure),
            "pass" if payload.len() >= 24 => {
                let (nonce, encrypted) = payload.split_at(24);
                let plain = XChaCha20Poly1305::new(output_key.into())
                    .decrypt(
                        XNonce::from_slice(nonce),
                        Payload {
                            msg: encrypted,
                            aad: header,
                        },
                    )
                    .ok()?;
                let length = u32::from_le_bytes(plain.get(..4)?.try_into().ok()?) as usize;
                let stdout = plain.get(4..4 + length)?;
                let stderr = plain.get(4 + length..)?;
                (stdout.len() + stderr.len() <= OUTPUT_LIMIT)
                    .then(|| Entry::Pass((stdout.to_vec(), stderr.to_vec())))
            }
            _ => None,
        }
    }

    pub fn save(
        &self,
        entry: &Entry,
        output_key: &[u8; 32],
        binary: blake3::Hash,
        duration: Duration,
    ) -> io::Result<bool> {
        let status = match entry {
            Entry::Pass(_) => "pass",
            Entry::Impure => "impure",
        };
        let mut bytes = serde_json::to_vec(&json!({
            "version": 1, "mode": "gate", "status": status,
            "key": self.key.to_hex().as_str(),
            "identity": self.identity.to_hex().as_str(),
            "binary": binary.to_hex().as_str(),
            "host": fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim_end(),
            "duration_ns": duration.as_nanos() as u64,
            "stored_ns": crate::store::clock().as_nanos() as u64,
        }))?;
        bytes.push(b'\n');
        if let Entry::Pass((stdout, stderr)) = entry {
            if stdout.len() + stderr.len() > OUTPUT_LIMIT {
                return Ok(false);
            }
            let mut plain = Vec::new();
            plain.extend_from_slice(&(stdout.len() as u32).to_le_bytes());
            plain.extend(stdout);
            plain.extend(stderr);
            // A test can print its keyed environment. Bind both streams to
            // that environment and the header without persisting plaintext.
            let mut nonce = [0; 24];
            OsRng
                .try_fill_bytes(&mut nonce)
                .map_err(|error| io::Error::other(error.to_string()))?;
            let encrypted = XChaCha20Poly1305::new(output_key.into())
                .encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: &plain,
                        aad: &bytes,
                    },
                )
                .map_err(|_| io::Error::other("could not encrypt test output"))?;
            bytes.extend_from_slice(&nonce);
            bytes.extend_from_slice(&encrypted);
        }
        let path = self.path();
        let _guard = lock(&path.with_file_name(".lock"))?;
        if self.quarantined()? {
            return Ok(false);
        }
        // A concurrent clean run must not overwrite evidence of impurity.
        if matches!(entry, Entry::Pass(_))
            && matches!(self.lookup(output_key)?, Some(Entry::Impure))
        {
            return Ok(false);
        }
        make_space(&path, bytes.len() as u64, SHARD_LIMIT)?;
        crate::store::replace(&path, &bytes)?;
        Ok(true)
    }

    pub fn verify_hit(&self, run_id: &OsStr) -> io::Result<bool> {
        // The dispatcher has no list of future hits. Reserve the first twenty
        // across all runners, then sample the remaining hits by run and key.
        let run = blake3::hash(run_id.as_bytes());
        let mut counter = lock(&self.run_dir(run_id).join("hits"))?;
        let mut bytes = [0; 8];
        let hits = if counter.metadata()?.len() == 0 {
            0
        } else {
            counter.read_exact(&mut bytes)?;
            u64::from_le_bytes(bytes)
        };
        if hits < MIN_VERIFIED {
            counter.rewind()?;
            counter.write_all(&(hits + 1).to_le_bytes())?;
            return Ok(true);
        }
        let phase = blake3::keyed_hash(run.as_bytes(), self.key.as_bytes());
        let sample = u64::from_le_bytes(phase.as_bytes()[..8].try_into().unwrap());
        Ok(sample % 20 == 0)
    }
}

fn make_space(replacing: &Path, incoming: u64, limit: u64) -> io::Result<()> {
    let mut files = Vec::new();
    let mut total = incoming;
    for entry in fs::read_dir(replacing.parent().unwrap())? {
        let entry = entry?;
        let path = entry.path();
        if path == replacing
            || entry.file_name().len() != 64
            || blake3::Hash::from_hex(entry.file_name().as_encoded_bytes()).is_err()
        {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_file() {
            let modified = meta.modified()?;
            if recent(modified, EXPIRY) {
                total = total.saturating_add(meta.len());
                files.push((modified, meta.len(), path));
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    if total > limit {
        files.sort_unstable();
        for (_, size, path) in files {
            fs::remove_file(path)?;
            total -= size;
            if total <= limit {
                break;
            }
        }
    }
    if total > limit {
        return Err(io::Error::other(
            "test result exceeds cache shard size limit",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_evicts_oldest_and_keeps_replacement_space() -> io::Result<()> {
        let temp = crate::sandbox::Temp::new(Path::new(env!("CARGO_MANIFEST_DIR")))?;
        let path = |name: &[u8]| temp.0.join(blake3::hash(name).to_hex().as_str());
        let old = path(b"old");
        let newer = path(b"newer");
        let replacing = path(b"replacing");
        for entry in [&old, &newer, &replacing] {
            fs::write(entry, [0; 4])?;
        }
        File::open(&old)?.set_modified(SystemTime::now() - Duration::from_secs(60))?;
        make_space(&replacing, 4, 8)?;
        assert!(!old.exists());
        assert!(newer.exists());
        Ok(())
    }

    #[test]
    fn verification_reserves_twenty_hits_and_samples_by_run_id() -> io::Result<()> {
        let temp = crate::sandbox::Temp::new(Path::new(env!("CARGO_MANIFEST_DIR")))?;
        let mut store = Store {
            root: &temp.0,
            key: blake3::hash(b"key"),
            identity: blake3::hash(b"id"),
        };
        let run = OsStr::new("nextest");
        for _ in 0..20 {
            assert!(store.verify_hit(run)?);
        }
        let mut selected = 0;
        for index in 0u32..1_000 {
            store.key = blake3::hash(&index.to_le_bytes());
            let verify = store.verify_hit(run)?;
            assert_eq!(store.verify_hit(run)?, verify);
            selected += usize::from(verify);
        }
        assert!((30..70).contains(&selected), "{selected}");
        assert!(store.verify_hit(OsStr::new("other-nextest"))?);
        Ok(())
    }
}
