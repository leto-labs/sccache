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

use super::{ArgCodegen, CodeGen, ParsedArguments};
use crate::compiler::args::Argument;
use crate::util::{Digest, HashToDigest};
use fs_err as fs;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::hash::Hash;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Clone, Debug)]
pub(super) struct IncrementalSession {
    dir: PathBuf,
    crate_prefix: String,
    marker_name: String,
}

pub(super) type SessionSnapshot = HashMap<String, HashSet<OsString>>;

impl IncrementalSession {
    pub(super) fn new(
        args: &ParsedArguments,
        dir: PathBuf,
        version: &str,
        compiler_shlibs_digests: &[String],
    ) -> Self {
        let mut metadata: Vec<_> = args
            .arguments
            .iter()
            .filter_map(|arg| match arg.get_data() {
                Some(CodeGen(ArgCodegen { opt, value })) if opt == "metadata" => Some(value),
                _ => None,
            })
            .collect();
        metadata.sort_unstable();
        let is_test = args
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::UnknownFlag(flag) if flag == "--test"));
        let mut digest = Digest::new();
        (
            &args.crate_name,
            metadata,
            &args.crate_types,
            is_test,
            version,
            compiler_shlibs_digests,
        )
            .hash(&mut HashToDigest {
                digest: &mut digest,
            });

        let hash = digest.finish();
        let crate_prefix = format!("{}-", args.crate_name);
        let marker_name = format!("{crate_prefix}sccache-{}", &hash[..16]);
        Self {
            dir,
            crate_prefix,
            marker_name,
        }
    }

    pub(super) fn marker_path(&self) -> PathBuf {
        self.dir.join(&self.marker_name)
    }

    pub(super) fn has_session(&self) -> bool {
        self.read_marker()
            .and_then(|name| finalized_sessions(&self.dir.join(name)))
            .is_ok_and(|sessions| !sessions.is_empty())
    }

    fn read_marker(&self) -> io::Result<String> {
        let path = self.marker_path();
        let metadata = fs::symlink_metadata(&path)?;
        // Rustc uses the crate name followed by a short ID.
        let max_len = self.crate_prefix.len() + 64;
        if !metadata.is_file() || metadata.len() > max_len as u64 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut name = String::new();
        fs::File::open(&path)?
            .take(max_len as u64 + 1)
            .read_to_string(&mut name)?;
        if name.len() > max_len
            || !name.starts_with(&self.crate_prefix)
            || Path::new(&name)
                .file_name()
                .is_none_or(|file| file != name.as_str())
            || !fs::symlink_metadata(self.dir.join(&name))?.is_dir()
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(name)
    }

    pub(super) fn snapshot(&self) -> io::Result<SessionSnapshot> {
        let dirs = match fs::read_dir(&self.dir) {
            Ok(dirs) => dirs,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(err) => return Err(err),
        };
        let mut sessions = HashMap::new();
        for entry in dirs {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with(&self.crate_prefix) && entry.file_type()?.is_dir() {
                sessions.insert(name, finalized_sessions(&entry.path())?);
            }
        }
        Ok(sessions)
    }

    pub(super) fn store_after_compile(&self, before: io::Result<SessionSnapshot>) -> bool {
        match before.and_then(|before| self.observe(&before)) {
            Ok(true) => true,
            Ok(false) => {
                debug!(
                    "[{}]: incremental compile did not confirm a cold session",
                    self.crate_prefix
                );
                false
            }
            Err(err) => {
                debug!(
                    "[{}]: could not observe incremental sessions: {err}",
                    self.crate_prefix
                );
                false
            }
        }
    }

    fn observe(&self, before: &SessionSnapshot) -> io::Result<bool> {
        let after = self.snapshot()?;
        let mut changed = after.iter().filter_map(|(name, sessions)| {
            let previous = before.get(name);
            sessions
                .iter()
                .any(|s| previous.is_none_or(|p| !p.contains(s)))
                .then_some((name, previous.is_none_or(HashSet::is_empty)))
        });
        let Some((folder, cold)) = changed.next() else {
            return Ok(false);
        };
        if changed.next().is_some() {
            return Ok(false);
        }
        if let Err(err) = self.write_marker(folder) {
            debug!(
                "[{}]: could not write incremental session marker: {err}",
                self.crate_prefix
            );
        }
        Ok(cold)
    }

    fn write_marker(&self, folder: &str) -> io::Result<()> {
        let mut marker = NamedTempFile::new_in(&self.dir)?;
        marker.write_all(folder.as_bytes())?;
        marker.persist(self.marker_path())?;
        Ok(())
    }
}

fn finalized_sessions(dir: &Path) -> io::Result<HashSet<OsString>> {
    let mut sessions = HashSet::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.starts_with(b"s-") && !bytes.ends_with(b"-working") && entry.file_type()?.is_dir()
        {
            sessions.insert(name);
        }
    }
    Ok(sessions)
}
