use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use libc as c;

use crate::{observer::Rules, process::Capture, store};

const EXECUTE: u64 = 1 << 0;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const IOCTL_DEV: u64 = 1 << 15;
const HANDLED_FS: u64 = (IOCTL_DEV << 1) - 1;
const TEMP_RIGHTS: u64 = HANDLED_FS & !(EXECUTE | IOCTL_DEV);
const HANDLED_NET: u64 = 3; // TCP bind and connect.
const SCOPED: u64 = 3; // Abstract Unix sockets and signals.
const MIN_ABI: i64 = 6;

const SYSTEM_PATHS: &[(&str, u64)] = &[
    ("/etc/ld.so.cache", READ_FILE),
    ("/proc", READ_FILE | READ_DIR),
    ("/sys/fs/cgroup", READ_FILE | READ_DIR),
    ("/dev/null", READ_FILE),
    ("/dev/urandom", READ_FILE),
];

const KILLED_SYSCALLS: &[c::c_long] = &[
    c::SYS_execve,
    c::SYS_io_uring_setup,
    c::SYS_open_by_handle_at,
    c::SYS_pidfd_getfd,
    c::SYS_ptrace,
    c::SYS_process_vm_readv,
    c::SYS_process_vm_writev,
    c::SYS_seccomp,
];

pub fn key(shadow: blake3::Hash) -> blake3::Hash {
    let mut profile = blake3::Hasher::new_derive_key("leto test cache stage 1 profile");
    profile.update(b"v1:metadata-observer,filtered-env,null-stdin,execveat-once,socket-eacces");
    for rights in [HANDLED_FS, TEMP_RIGHTS, HANDLED_NET, SCOPED, MIN_ABI as u64] {
        profile.update(&rights.to_le_bytes());
    }
    for (path, rights) in SYSTEM_PATHS {
        profile.update(path.as_bytes());
        profile.update(&[0]);
        profile.update(&rights.to_le_bytes());
    }
    for instruction in filter() {
        profile.update(&instruction.code.to_le_bytes());
        profile.update(&[instruction.jt, instruction.jf]);
        profile.update(&instruction.k.to_le_bytes());
    }
    let mut hash = blake3::Hasher::new_derive_key("leto test cache stage 1 result key");
    hash.update(shadow.as_bytes());
    hash.update(profile.finalize().as_bytes());
    hash.finalize()
}

pub fn check_abi() -> io::Result<()> {
    // SAFETY: a null attribute with size zero and the version flag queries the ABI.
    let abi = unsafe {
        c::syscall(
            c::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            1u32,
        )
    };
    if abi < 0 {
        return Err(io::Error::other(format!(
            "Landlock unavailable: {}",
            io::Error::last_os_error()
        )));
    }
    if abi < MIN_ABI {
        return Err(io::Error::other(format!(
            "Landlock ABI {abi} is below required ABI {MIN_ABI}"
        )));
    }
    Ok(())
}

#[repr(C, packed)]
struct PathRule {
    allowed_access: u64,
    parent_fd: c::c_int,
}

fn restrict_files(rules: &Rules, executable: &File) -> io::Result<()> {
    #[repr(C)]
    struct Ruleset {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }
    let attr = Ruleset {
        handled_access_fs: HANDLED_FS,
        handled_access_net: HANDLED_NET,
        scoped: SCOPED,
    };
    // SAFETY: the kernel copies the three ABI 6 ruleset fields.
    let fd = unsafe {
        c::syscall(
            c::SYS_landlock_create_ruleset,
            &attr,
            std::mem::size_of_val(&attr),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful syscall returned a new, owned descriptor.
    let ruleset = unsafe { File::from_raw_fd(fd as c::c_int) };
    let allow = |file: &File, access| -> io::Result<()> {
        let path_rule = PathRule {
            allowed_access: access,
            parent_fd: file.as_raw_fd(),
        };
        // SAFETY: both descriptors are live and the kernel copies the packed path rule.
        if unsafe {
            c::syscall(
                c::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                1u32, // LANDLOCK_RULE_PATH_BENEATH
                &path_rule,
                0u32,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    allow(executable, READ_FILE | EXECUTE)?;
    for path in &rules.libraries {
        let access = if path == Path::new(crate::key::LOADER) {
            READ_FILE | EXECUTE
        } else {
            READ_FILE
        };
        allow(&open_path(path)?, access)?;
    }
    for &(path, access) in SYSTEM_PATHS {
        match open_path(Path::new(path)) {
            Ok(file) => allow(&file, access)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    allow(&open_path(&rules.temp)?, TEMP_RIGHTS)?;
    // SAFETY: the ruleset descriptor is live and no_new_privs is required before applying it.
    if unsafe {
        c::prctl(c::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
            || c::syscall(c::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) < 0
    } {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_path(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(c::O_PATH)
        .open(path)
}

fn filter() -> Vec<c::sock_filter> {
    let stmt = |code, k| c::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code, k, jt, jf| c::sock_filter { code, jt, jf, k };
    let kill = stmt(0x06, c::SECCOMP_RET_KILL_PROCESS);
    let mut code = vec![
        stmt(0x20, 4),                 // seccomp_data.arch
        jump(0x15, 0xc000_003e, 1, 0), // AUDIT_ARCH_X86_64
        kill,
        stmt(0x20, 0),                 // seccomp_data.nr
        jump(0x45, 0x4000_0000, 0, 1), // x32 syscall bit
        kill,
        jump(0x15, c::SYS_socket as u32, 0, 1),
        stmt(0x06, c::SECCOMP_RET_TRAP),
    ];
    for &number in KILLED_SYSCALLS {
        code.push(jump(0x15, number as u32, 0, 1));
        code.push(kill);
    }
    code.push(stmt(0x06, c::SECCOMP_RET_ALLOW));
    code
}

pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new(root: &Path) -> io::Result<Self> {
        let base = std::env::var_os("TMPDIR").map_or_else(|| root.join("tmp"), PathBuf::from);
        if std::env::var_os("TMPDIR").is_none() {
            fs::create_dir_all(&base)?;
        }
        let path = base.canonicalize()?.join(format!(
            "leto-test-{}-{}",
            std::process::id(),
            store::clock().as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct Launch<'a> {
    executable: File,
    argv: Vec<CString>,
    env: Vec<CString>,
    capture: &'a Capture,
    stdin: File,
}

impl<'a> Launch<'a> {
    pub fn new(
        binary: &Path,
        args: &[OsString],
        mut environment: BTreeMap<OsString, OsString>,
        temp: &Path,
        capture: &'a Capture,
    ) -> io::Result<Self> {
        environment.insert("TMPDIR".into(), temp.as_os_str().to_owned());
        let argv = std::iter::once(binary.as_os_str())
            .chain(args.iter().map(OsString::as_os_str))
            .map(|arg| CString::new(arg.as_bytes()))
            .collect::<Result<_, _>>()?;
        let env = environment
            .into_iter()
            .map(|(name, value)| {
                let mut pair = name.into_encoded_bytes();
                pair.push(b'=');
                pair.extend_from_slice(value.as_bytes());
                CString::new(pair)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            executable: open_path(binary)?,
            argv,
            env,
            capture,
            stdin: File::open("/dev/null")?,
        })
    }

    pub fn prepare(&self, rules: &Rules) -> io::Result<()> {
        self.capture.redirect(&self.stdin)?;
        // SAFETY: close_range marks open descriptors without closing them before execveat.
        if unsafe { c::syscall(c::SYS_close_range, 3u32, u32::MAX, c::CLOSE_RANGE_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        restrict_files(rules, &self.executable)?;
        crate::observer::install_filter(filter())
    }

    pub fn exec(&self) -> ! {
        let pointers = |strings: &[CString]| {
            strings
                .iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect::<Vec<_>>()
        };
        let argv = pointers(&self.argv);
        let env = pointers(&self.env);
        // SAFETY: the executable fd and NUL-terminated argument and environment arrays
        // remain live until the syscall replaces the image. No fallback is launched here.
        unsafe {
            c::syscall(
                c::SYS_execveat,
                self.executable.as_raw_fd(),
                c"".as_ptr(),
                argv.as_ptr(),
                env.as_ptr(),
                c::AT_EMPTY_PATH,
            );
            c::_exit(126);
        }
    }
}
