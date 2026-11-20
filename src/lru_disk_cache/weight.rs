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

use std::{fs::File, path::Path};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    const WEIGHT: &CStr = c"user.sccache.weight";
    const COMPRESSION: &CStr = c"btrfs.compression";

    fn get_attr<'a>(path: &CStr, name: &CStr, value: &'a mut [u8]) -> Option<&'a [u8]> {
        // SAFETY: Both strings are NUL-terminated and value is writable for its length.
        let len = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        value.get(..usize::try_from(len).ok()?)
    }

    fn uses_zstd(property: &[u8]) -> bool {
        property == b"zstd"
            || property.strip_prefix(b"zstd:").is_some_and(|level| {
                std::str::from_utf8(level)
                    .ok()
                    .and_then(|level| level.parse::<u8>().ok())
                    .is_some_and(|level| level <= 15)
            })
    }

    pub(crate) fn detect(path: &Path) -> bool {
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        is_btrfs(&path) && get_attr(&path, COMPRESSION, &mut [0; 16]).is_some_and(uses_zstd)
    }

    fn is_btrfs(path: &CStr) -> bool {
        let mut stat = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: statfs initializes the struct on success, using the NUL-terminated path.
        let stat = unsafe {
            if libc::statfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
                return false;
            }
            stat.assume_init()
        };
        // The estimator assumes 4 KiB btrfs sectors and zstd compression.
        stat.f_type == libc::BTRFS_SUPER_MAGIC && stat.f_bsize == 4096
    }

    pub(crate) fn read(path: &Path, length: u64) -> u64 {
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return length;
        };
        get_attr(&path, WEIGHT, &mut [0; 8])
            .and_then(|value| value.try_into().ok())
            .map(u64::from_le_bytes)
            .filter(|&weight| weight > 0 && weight < length)
            .unwrap_or(length)
    }

    pub(crate) fn write(file: &File, weight: u64) -> bool {
        let mut compression = [0; 16];
        // SAFETY: The file is open and compression is writable for its length.
        let len = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                COMPRESSION.as_ptr(),
                compression.as_mut_ptr().cast(),
                compression.len(),
            )
        };
        if !usize::try_from(len)
            .ok()
            .and_then(|len| compression.get(..len))
            .is_some_and(uses_zstd)
        {
            return false;
        }
        let value = weight.to_le_bytes();
        // SAFETY: The file is open, the name is NUL-terminated and value holds eight bytes.
        (unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                WEIGHT.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        }) == 0
    }

    #[cfg(test)]
    pub(crate) fn test_btrfs_zstd(path: &Path) -> bool {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        if !is_btrfs(&path) {
            eprintln!("skipping weighted disk cache check: requires 4 KiB btrfs");
            return false;
        }
        let set_compression = |property: &[u8]| {
            // SAFETY: Both strings are NUL-terminated and property is readable for its length.
            unsafe {
                libc::lsetxattr(
                    path.as_ptr(),
                    COMPRESSION.as_ptr(),
                    property.as_ptr().cast(),
                    property.len(),
                    0,
                )
            }
        };
        // Older btrfs kernels accept an algorithm without a level.
        if set_compression(b"zstd:1") != 0 {
            assert_eq!(
                set_compression(b"zstd"),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
        true
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::test_btrfs_zstd;

#[cfg(target_os = "linux")]
pub(super) use linux::{detect, read, write};

#[cfg(not(target_os = "linux"))]
pub(super) fn detect(_path: &Path) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub(super) fn read(_path: &Path, length: u64) -> u64 {
    length
}

#[cfg(not(target_os = "linux"))]
pub(super) fn write(_file: &File, _weight: u64) -> bool {
    false
}
