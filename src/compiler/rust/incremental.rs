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
use filetime::FileTime;
use std::hash::Hash;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub(crate) struct IncrementalSession {
    path: PathBuf,
}

impl IncrementalSession {
    pub(super) fn from_parsed(args: &ParsedArguments) -> Option<Self> {
        let metadata = args
            .arguments
            .iter()
            .filter_map(|arg| match arg.get_data() {
                Some(CodeGen(ArgCodegen {
                    opt,
                    value: Some(value),
                })) if opt == "metadata" => Some(value.as_str()),
                _ => None,
            })
            .collect();
        let is_test = args
            .arguments
            .iter()
            .any(|arg| matches!(arg, Argument::UnknownFlag(flag) if flag == "--test"));
        let crate_types: Vec<_> = [
            args.crate_types.rlib.then_some("rlib"),
            args.crate_types.staticlib.then_some("staticlib"),
        ]
        .into_iter()
        .flatten()
        .collect();
        Some(Self::new(
            args.incremental_dir.as_deref()?,
            &args.crate_name,
            metadata,
            &crate_types,
            is_test,
        ))
    }

    fn new(
        dir: &Path,
        crate_name: &str,
        mut metadata: Vec<&str>,
        crate_types: &[&str],
        is_test: bool,
    ) -> Self {
        metadata.sort_unstable();
        let mut digest = Digest::new();
        digest.delimiter(b"rust-incremental-argv");
        (crate_name, metadata, crate_types, is_test).hash(&mut HashToDigest {
            digest: &mut digest,
        });
        let hash = digest.finish();
        Self {
            path: dir.join(format!("{crate_name}-sccache-{}", &hash[..16])),
        }
    }

    pub(crate) fn refresh_mtime(&self) {
        // Cleanup tools use the mtime of the top-level incremental directory entry.
        let _ = filetime::set_file_mtime(&self.path, FileTime::now());
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
