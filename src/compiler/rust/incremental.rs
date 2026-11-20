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
use std::hash::Hash;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub(super) struct IncrementalSession {
    path: PathBuf,
}

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
        digest.delimiter(b"rust-incremental-dir");
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
        let path = dir.join(format!("{}-sccache-{}", args.crate_name, &hash[..16]));
        Self { path }
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
            let name = entry.file_name().as_encoded_bytes();
            if entry.depth() > 0
                && entry.file_type().is_dir()
                && name.starts_with(b"s-")
                && !name.ends_with(b"-working")
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
