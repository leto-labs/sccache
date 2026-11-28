use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use libc as c;
use serde_json::{Value, json};

use crate::process;

pub const PROFILE: &str = "linux-x86_64-observer-v2:binary,closure,ld-cache,proc,cgroup,null,urandom,tmp,ancestor-lookups";
const AT_CWD: i32 = c::AT_FDCWD;

#[derive(Clone, Copy)]
enum Decode {
    Open,
    OpenAt,
    OpenAt2,
    Metadata,
    MetadataAt,
    MetadataFd,
    Read,
    ReadFd,
    ListFd,
    Write,
    WriteAt,
    WriteFd,
    Rename,
    RenameAt,
    Link,
    LinkAt,
    Symlink,
    SymlinkAt,
    Socket,
    Address,
    SendTo,
    Exec,
    ExecAt,
    Process,
    Other,
}

use Decode::*;

const SYSCALLS: &[(c::c_long, Decode)] = &[
    (c::SYS_open, Open),
    (c::SYS_creat, Write),
    (c::SYS_openat, OpenAt),
    (c::SYS_openat2, OpenAt2),
    (c::SYS_stat, Metadata),
    (c::SYS_lstat, Metadata),
    (c::SYS_fstat, MetadataFd),
    (c::SYS_newfstatat, MetadataAt),
    (c::SYS_statx, MetadataAt),
    (c::SYS_statfs, Read),
    (c::SYS_fstatfs, ReadFd),
    (c::SYS_access, Metadata),
    (c::SYS_faccessat, MetadataAt),
    (c::SYS_faccessat2, MetadataAt),
    (c::SYS_readlink, Metadata),
    (c::SYS_readlinkat, MetadataAt),
    (c::SYS_getdents, ListFd),
    (c::SYS_getdents64, ListFd),
    (c::SYS_chdir, Metadata),
    (c::SYS_fchdir, MetadataFd),
    (c::SYS_getxattr, Read),
    (c::SYS_lgetxattr, Read),
    (c::SYS_fgetxattr, ReadFd),
    (c::SYS_listxattr, Read),
    (c::SYS_llistxattr, Read),
    (c::SYS_flistxattr, ReadFd),
    (c::SYS_unlink, Write),
    (c::SYS_unlinkat, WriteAt),
    (c::SYS_mkdir, Write),
    (c::SYS_mkdirat, WriteAt),
    (c::SYS_rmdir, Write),
    (c::SYS_mknod, Write),
    (c::SYS_mknodat, WriteAt),
    (c::SYS_chmod, Write),
    (c::SYS_fchmod, WriteFd),
    (c::SYS_fchmodat, WriteAt),
    (c::SYS_fchmodat2, WriteAt),
    (c::SYS_chown, Write),
    (c::SYS_lchown, Write),
    (c::SYS_fchown, WriteFd),
    (c::SYS_fchownat, WriteAt),
    (c::SYS_truncate, Write),
    (c::SYS_ftruncate, WriteFd),
    (c::SYS_utime, Write),
    (c::SYS_utimes, Write),
    (c::SYS_futimesat, WriteAt),
    (c::SYS_utimensat, WriteAt),
    (c::SYS_setxattr, Write),
    (c::SYS_lsetxattr, Write),
    (c::SYS_fsetxattr, WriteFd),
    (c::SYS_removexattr, Write),
    (c::SYS_lremovexattr, Write),
    (c::SYS_fremovexattr, WriteFd),
    (c::SYS_rename, Rename),
    (c::SYS_renameat, RenameAt),
    (c::SYS_renameat2, RenameAt),
    (c::SYS_link, Link),
    (c::SYS_linkat, LinkAt),
    (c::SYS_symlink, Symlink),
    (c::SYS_symlinkat, SymlinkAt),
    (c::SYS_socket, Socket),
    (c::SYS_connect, Address),
    (c::SYS_bind, Address),
    (c::SYS_sendto, SendTo),
    (c::SYS_execve, Exec),
    (c::SYS_execveat, ExecAt),
    (c::SYS_fork, Process),
    (c::SYS_vfork, Process),
    (c::SYS_clone, Process),
    (c::SYS_clone3, Process),
    (c::SYS_io_uring_setup, Other),
    (c::SYS_open_by_handle_at, Other),
    (c::SYS_name_to_handle_at, Other),
];

pub fn filter() -> Vec<c::sock_filter> {
    const LOAD: u16 = 0x20;
    const EQUAL: u16 = 0x15;
    const RETURN: u16 = 0x06;
    let stmt = |code, k| c::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let eq = |k, jt, jf| c::sock_filter {
        code: EQUAL,
        jt,
        jf,
        k,
    };
    let mut code = vec![
        stmt(LOAD, 4),         // seccomp_data.arch
        eq(0xc000_003e, 1, 0), // AUDIT_ARCH_X86_64
        stmt(RETURN, c::SECCOMP_RET_ALLOW),
        stmt(LOAD, 0), // seccomp_data.nr
    ];
    for (index, (number, _)) in SYSCALLS.iter().enumerate() {
        code.push(eq(*number as u32, 0, 1));
        code.push(stmt(RETURN, c::SECCOMP_RET_TRACE | index as u32));
    }
    code.push(stmt(RETURN, c::SECCOMP_RET_ALLOW));
    code
}

pub fn install() -> io::Result<()> {
    install_filter(filter())
}

pub fn install_filter(mut code: Vec<c::sock_filter>) -> io::Result<()> {
    let mut program = c::sock_fprog {
        len: code.len() as u16,
        filter: code.as_mut_ptr(),
    };
    // SAFETY: the kernel copies the initialized BPF instructions during prctl.
    if unsafe {
        c::prctl(c::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
            || c::prctl(c::PR_SET_SECCOMP, c::SECCOMP_MODE_FILTER, &mut program) < 0
    } {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ptrace(operation: c::c_uint, pid: c::pid_t, data: *mut c::c_void) -> io::Result<()> {
    // SAFETY: these ptrace operations use a zero address. Callers provide
    // initialized buffers or the integer-valued data prescribed by ptrace.
    if unsafe { c::ptrace(operation, pid, std::ptr::null::<c::c_void>(), data) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn attach(pid: c::pid_t, sandboxed: bool) -> io::Result<()> {
    let mut options = c::PTRACE_O_TRACESECCOMP
        | c::PTRACE_O_TRACESYSGOOD
        | c::PTRACE_O_TRACECLONE
        | c::PTRACE_O_TRACEVFORK
        | c::PTRACE_O_TRACEFORK
        | c::PTRACE_O_TRACEEXEC;
    if sandboxed {
        options |= c::PTRACE_O_EXITKILL;
    }
    ptrace(c::PTRACE_SEIZE, pid, options as usize as *mut _)
}

fn message(pid: c::pid_t) -> io::Result<usize> {
    let mut data: c::c_ulong = 0;
    ptrace(
        c::PTRACE_GETEVENTMSG,
        pid,
        (&mut data as *mut c::c_ulong).cast(),
    )?;
    Ok(data as usize)
}

fn seccomp_stop(pid: c::pid_t) -> io::Result<(usize, u64, [u64; 6])> {
    // SAFETY: the kernel copies syscall information into our initialized buffer.
    let mut info: c::ptrace_syscall_info = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info);
    let count = unsafe { c::ptrace(c::PTRACE_GET_SYSCALL_INFO, pid, size, &mut info) };
    let min_bytes = std::mem::offset_of!(c::ptrace_syscall_info, u)
        + std::mem::offset_of!(c::__c_anonymous_ptrace_syscall_info_seccomp, ret_data)
        + std::mem::size_of::<u32>();
    if count >= min_bytes as c::c_long && info.op == c::PTRACE_SYSCALL_INFO_SECCOMP {
        // SAFETY: the kernel tagged the union as a complete seccomp stop.
        let call = unsafe { info.u.seccomp };
        return Ok((call.ret_data as usize, info.instruction_pointer, call.args));
    }
    let regs = registers(pid)?;
    Ok((
        message(pid)?,
        regs.rip,
        [regs.rdi, regs.rsi, regs.rdx, regs.r10, regs.r8, regs.r9],
    ))
}

fn registers(pid: c::pid_t) -> io::Result<c::user_regs_struct> {
    // SAFETY: integer registers can be initialized to zero before GETREGS fills them.
    let mut regs: c::user_regs_struct = unsafe { std::mem::zeroed() };
    ptrace(
        c::PTRACE_GETREGS,
        pid,
        (&mut regs as *mut c::user_regs_struct).cast(),
    )?;
    Ok(regs)
}

pub struct Rules {
    pub checkout: PathBuf,
    pub temp: PathBuf,
    pub binary: PathBuf,
    pub libraries: Vec<PathBuf>,
}

impl Rules {
    pub fn canonicalize(&mut self) {
        for path in [&mut self.checkout, &mut self.temp, &mut self.binary]
            .into_iter()
            .chain(self.libraries.iter_mut())
        {
            if let Ok(real) = path.canonicalize() {
                *path = real;
            }
        }
    }

    fn is_ancestor(&self, path: &Path) -> bool {
        path == Path::new("/")
            || (path != self.checkout && self.checkout.starts_with(path))
            || (path != self.temp && self.temp.starts_with(path))
    }
}

#[derive(Default)]
struct Trace {
    checkout_reads: BTreeSet<String>,
    system_reads: BTreeSet<String>,
    other_reads: BTreeSet<String>,
    outside_writes: BTreeSet<String>,
    absent_probes: BTreeSet<String>,
    loader_probes: BTreeSet<String>,
    ancestor_lookups: BTreeSet<String>,
    sockets: BTreeSet<(i32, Option<bool>)>,
    executables: BTreeSet<String>,
    files: bool,
    exec: bool,
    network: bool,
    reason: Option<&'static str>,
    budget: usize,
    loader_budget: usize,
    system_budget: usize,
    truncated: bool,
    denied: bool,
}

impl Trace {
    fn new() -> Self {
        Self {
            budget: 900,
            loader_budget: 240,
            system_budget: 500,
            ..Self::default()
        }
    }

    fn record_path(&mut self, access: Access, result: i64, rules: &Rules, sandboxed: bool) {
        if sandboxed
            && [c::EACCES, c::EPERM, c::EXDEV]
                .iter()
                .any(|&errno| result == -i64::from(errno))
        {
            self.denied = true;
            self.reason.get_or_insert("sandbox access denied");
        }
        let path = access.path;
        let absent = result == -i64::from(c::ENOENT) || result == -i64::from(c::ENOTDIR);
        let proc_path = ["/proc/self", "/proc/thread-self"]
            .iter()
            .find_map(|prefix| {
                path.strip_prefix(prefix)
                    .ok()
                    .map(|tail| PathBuf::from(format!("/proc/{}", access.pid)).join(tail))
            });
        let resolved = resolve(proc_path.as_deref().unwrap_or(&path));
        if sandboxed && resolved.starts_with(&rules.temp) {
            return;
        }
        if access.kind == Lookup && !absent && rules.is_ancestor(&resolved) {
            insert(
                &mut self.ancestor_lookups,
                &resolved.to_string_lossy(),
                &mut self.budget,
                &mut self.truncated,
            );
            return;
        }
        let display = path
            .strip_prefix(&rules.checkout)
            .ok()
            .map(|p| Path::new(".").join(p));
        let text = display.as_deref().unwrap_or(&path).to_string_lossy();
        if access.loader
            && matches!(access.kind, Lookup | ReadContent)
            && (absent || resolved.is_dir())
        {
            let paths = if absent {
                &mut self.absent_probes
            } else {
                &mut self.loader_probes
            };
            insert(paths, &text, &mut self.loader_budget, &mut self.truncated);
            return;
        }
        if absent {
            insert(
                &mut self.absent_probes,
                &text,
                &mut self.budget,
                &mut self.truncated,
            );
        }
        let in_checkout = resolved.starts_with(&rules.checkout);
        let system = [
            "/etc", "/usr", "/lib", "/lib64", "/proc", "/sys", "/dev", "/run",
        ]
        .iter()
        .any(|base| resolved.starts_with(base));
        if access.kind == Mutate {
            if in_checkout || !resolved.starts_with(&rules.temp) {
                self.reason.get_or_insert("write outside TMPDIR");
                insert(
                    &mut self.outside_writes,
                    &text,
                    &mut self.budget,
                    &mut self.truncated,
                );
            }
        } else if resolved == rules.binary
            || rules.libraries.contains(&resolved)
            || Path::new("/etc/ld.so.cache") == resolved
            || resolved == Path::new("/dev/null")
            || resolved == Path::new("/dev/urandom")
            || resolved.starts_with("/proc")
            || resolved.starts_with("/sys/fs/cgroup")
            // Shadow mode preserves ambient TERM (whereas Stage 1 strips it),
            // so Rust libtest probes terminfo databases on every run.
            || (!sandboxed && is_terminfo(&resolved))
            || (system && absent)
        {
            if system && !absent {
                insert(
                    &mut self.system_reads,
                    &text,
                    &mut self.system_budget,
                    &mut self.truncated,
                );
            }
        } else if in_checkout {
            self.files = true;
            if !absent {
                insert(
                    &mut self.checkout_reads,
                    &text,
                    &mut self.budget,
                    &mut self.truncated,
                );
            }
        } else if !resolved.starts_with(&rules.temp) {
            self.reason
                .get_or_insert("read outside checkout and allowed system paths");
            if !absent {
                let (set, budget) = if system {
                    (&mut self.system_reads, &mut self.system_budget)
                } else {
                    (&mut self.other_reads, &mut self.budget)
                };
                insert(set, &text, budget, &mut self.truncated);
            }
        }
    }

    fn socket(&mut self, family: i32, loopback: Option<bool>) {
        self.network = true;
        if self.sockets.len() < 12 || self.sockets.contains(&(family, loopback)) {
            self.sockets.insert((family, loopback));
        } else {
            self.truncated = true;
        }
    }

    fn executable(&mut self, path: Option<&Path>) {
        self.exec = true;
        if let Some(path) = path {
            insert(
                &mut self.executables,
                &path.to_string_lossy(),
                &mut self.budget,
                &mut self.truncated,
            );
        }
    }

    fn evidence(&self, filtered: bool) -> Value {
        if !filtered {
            return json!("unavailable");
        }
        let class = if self.network {
            "network"
        } else if self.exec {
            "exec"
        } else if self.reason.is_some() {
            "other"
        } else if self.files {
            "files"
        } else {
            "pure"
        };
        let sockets: Vec<_> = self
            .sockets
            .iter()
            .map(|(family, loopback)| json!({"family": family, "loopback": loopback}))
            .collect();
        json!({
            "class": class, "reason": self.reason,
            "checkout_reads": self.checkout_reads, "system_reads": self.system_reads,
            "other_reads": self.other_reads, "outside_writes": self.outside_writes,
            "absent_probes": self.absent_probes, "loader_probes": self.loader_probes, "sockets": sockets,
            "ancestor_lookups": self.ancestor_lookups,
            "executables": self.executables, "truncated": self.truncated,
        })
    }
}

fn insert(set: &mut BTreeSet<String>, value: &str, budget: &mut usize, truncated: &mut bool) {
    if set.contains(value) {
        return;
    }
    let size = serde_json::to_string(value).unwrap().len() + 1;
    if let Some(remaining) = budget.checked_sub(size) {
        *budget = remaining;
        set.insert(value.to_owned());
    } else {
        *truncated = true;
    }
}

fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    for parent in path.ancestors().skip(1) {
        if let Ok(real) = parent.canonicalize()
            && let Ok(tail) = path.strip_prefix(parent)
        {
            return real.join(tail);
        }
    }
    path.to_path_buf()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathAccess {
    Lookup,
    ReadContent,
    ListDirectory,
    Mutate,
}

use PathAccess::*;

struct Access {
    pid: c::pid_t,
    path: PathBuf,
    kind: PathAccess,
    loader: bool,
}

fn read_memory(pid: c::pid_t, address: u64, bytes: &mut [u8]) -> io::Result<usize> {
    let local = c::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let remote = c::iovec {
        iov_base: address as usize as *mut _,
        iov_len: bytes.len(),
    };
    // SAFETY: local points to a valid writable slice. The kernel reads
    // remote addresses in the tracee and checks accessibility.
    let length = unsafe { c::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if length <= 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(length as usize)
}

fn cstring(pid: c::pid_t, address: u64) -> io::Result<OsString> {
    let mut path = Vec::new();
    while path.len() < 4096 {
        let at = address
            .checked_add(path.len() as u64)
            .ok_or_else(|| io::Error::other("invalid address"))?;
        let mut chunk = [0; 256];
        // A path can end immediately before a guard page.
        let size = chunk.len().min(4096 - at as usize % 4096);
        let length = read_memory(pid, at, &mut chunk[..size])?;
        if let Some(end) = chunk[..length].iter().position(|&b| b == 0) {
            path.extend_from_slice(&chunk[..end]);
            return Ok(OsString::from_vec(path));
        }
        path.extend_from_slice(&chunk[..length]);
    }
    Err(io::Error::other("unterminated syscall path"))
}

fn fd_path(pid: c::pid_t, fd: i32) -> io::Result<Option<PathBuf>> {
    let path = if fd == AT_CWD {
        format!("/proc/{pid}/cwd")
    } else {
        format!("/proc/{pid}/fd/{fd}")
    };
    match fs::read_link(path) {
        Ok(path) => Ok(path.is_absolute().then_some(path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn argument_path(pid: c::pid_t, dir: i32, address: u64) -> io::Result<Option<PathBuf>> {
    if address == 0 {
        return fd_path(pid, dir);
    }
    let path = PathBuf::from(cstring(pid, address)?);
    if path.is_absolute() {
        return Ok(Some(path));
    }
    Ok(fd_path(pid, dir)?.map(|base| {
        if path.as_os_str().is_empty() {
            base
        } else {
            base.join(path)
        }
    }))
}

fn open_access(flags: u64) -> PathAccess {
    if flags & c::O_PATH as u64 != 0 {
        Lookup
    } else if flags & ((c::O_ACCMODE | c::O_CREAT | c::O_TRUNC) as u64) != 0
        || flags & c::O_TMPFILE as u64 == c::O_TMPFILE as u64
    {
        Mutate
    } else if flags & c::O_DIRECTORY as u64 != 0 {
        Lookup
    } else {
        ReadContent
    }
}

fn address(pid: c::pid_t, trace: &mut Trace, ptr: u64, len: u64) -> io::Result<()> {
    if len < 2 || ptr == 0 {
        return Ok(());
    }
    let mut data = [0; 28];
    let size = (len as usize).min(data.len());
    let got = read_memory(pid, ptr, &mut data[..size])?;
    if got < 2 {
        return Err(io::Error::other("incomplete socket address"));
    }
    let family = i32::from(u16::from_ne_bytes([data[0], data[1]]));
    let loopback = match family {
        c::AF_INET if got >= 8 => Some(data[4] == 127),
        c::AF_INET6 if got >= 24 => {
            let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&data[8..24]).unwrap());
            Some(ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback()))
        }
        _ => None,
    };
    trace.socket(family, loopback);
    Ok(())
}

fn decode(
    pid: c::pid_t,
    kind: Decode,
    arg: [u64; 6],
    startup: bool,
    loader: bool,
    trace: &mut Trace,
) -> io::Result<Vec<Access>> {
    let [a, b, d, e, f, g] = arg;
    let mut paths = Vec::new();
    let mut add = |dir, ptr, kind| -> io::Result<()> {
        if let Some(path) = argument_path(pid, dir, ptr)? {
            paths.push(Access {
                pid,
                path,
                kind,
                loader,
            });
        }
        Ok(())
    };
    match kind {
        Open => add(AT_CWD, a, open_access(b))?,
        OpenAt => add(a as i32, b, open_access(d))?,
        OpenAt2 => {
            let mut flags = [0; 8];
            if read_memory(pid, d, &mut flags)? != flags.len() {
                return Err(io::Error::other("incomplete open_how"));
            }
            add(a as i32, b, open_access(u64::from_ne_bytes(flags)))?;
        }
        Metadata => add(AT_CWD, a, Lookup)?,
        MetadataAt => add(a as i32, b, Lookup)?,
        MetadataFd if a > 2 => add(a as i32, 0, Lookup)?,
        Read => add(AT_CWD, a, ReadContent)?,
        ReadFd if a > 2 => add(a as i32, 0, ReadContent)?,
        MetadataFd | ReadFd => {}
        ListFd => {
            // Resolve the live descriptor so dup, inheritance and fd reuse
            // do not depend on which open call originally supplied it.
            add(a as i32, 0, ListDirectory)?;
            if paths.is_empty() {
                trace
                    .reason
                    .get_or_insert("could not resolve directory listing");
            }
        }
        Write => add(AT_CWD, a, Mutate)?,
        WriteAt => add(a as i32, b, Mutate)?,
        WriteFd => add(a as i32, 0, Mutate)?,
        Rename | Link => {
            let source = if matches!(kind, Rename) {
                Mutate
            } else {
                ReadContent
            };
            add(AT_CWD, a, source)?;
            add(AT_CWD, b, Mutate)?;
        }
        RenameAt | LinkAt => {
            let source = if matches!(kind, RenameAt) {
                Mutate
            } else {
                ReadContent
            };
            add(a as i32, b, source)?;
            add(d as i32, e, Mutate)?;
        }
        Symlink => add(AT_CWD, b, Mutate)?,
        SymlinkAt => add(b as i32, d, Mutate)?,
        Socket => trace.socket(a as i32, None),
        Address => address(pid, trace, b, d)?,
        SendTo => address(pid, trace, f, g)?,
        Exec | ExecAt if !startup => {
            trace.exec = true;
            let path = if matches!(kind, Exec) {
                argument_path(pid, AT_CWD, a)?
            } else {
                argument_path(pid, a as i32, b)?
            };
            trace.executable(path.as_deref());
        }
        Exec | ExecAt | Process => {}
        Other => {
            trace.reason.get_or_insert("unresolved file activity");
        }
    }
    Ok(paths)
}

fn loader_text(pid: c::pid_t) -> Option<std::ops::Range<u64>> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    let line = maps.lines().find(|line| {
        line.ends_with("/ld-linux-x86-64.so.2")
            && line
                .split_whitespace()
                .nth(1)
                .is_some_and(|mode| mode.contains('x'))
    })?;
    let (start, end) = line.split_whitespace().next()?.split_once('-')?;
    Some(u64::from_str_radix(start, 16).ok()?..u64::from_str_radix(end, 16).ok()?)
}

pub fn collect(
    root: c::pid_t,
    mut rules: Rules,
    filtered: bool,
    sandboxed: bool,
    notify: impl FnOnce(i32, Value),
) {
    rules.canonicalize();
    let mut trace = Trace::new();
    let mut notify = Some(notify);
    let mut pending: HashMap<c::pid_t, Vec<Access>> = HashMap::new();
    let mut exec_started = false;
    let mut loaders = HashMap::new();
    let mut descendants = BTreeSet::new();
    while let Ok((pid, status)) = process::wait(-1, c::__WALL) {
        if c::WIFEXITED(status) || c::WIFSIGNALED(status) {
            pending.remove(&pid);
            loaders.remove(&pid);
            descendants.remove(&pid);
            if sandboxed && c::WIFSIGNALED(status) && c::WTERMSIG(status) == c::SIGSYS {
                trace.denied = true;
                trace.reason.get_or_insert("sandbox syscall denied");
            }
            if pid == root
                && let Some(notify) = notify.take()
            {
                if sandboxed && !exec_started {
                    trace.reason.get_or_insert("sandboxed test did not start");
                }
                if !descendants.is_empty() {
                    trace
                        .reason
                        .get_or_insert("descendants still running after test exit");
                }
                let mut evidence = trace.evidence(filtered);
                if sandboxed && filtered {
                    evidence["denied"] = json!(trace.denied);
                    evidence["started"] = json!(exec_started);
                }
                notify(status, evidence);
                if sandboxed {
                    // EXITKILL reaps any tracees that outlived the test.
                    return;
                }
            }
            continue;
        }
        if !c::WIFSTOPPED(status) {
            continue;
        }
        let event = status >> 16;
        let signal = c::WSTOPSIG(status);
        let mut deliver = 0;
        match event {
            c::PTRACE_EVENT_SECCOMP => {
                let decoded = seccomp_stop(pid).and_then(|(index, ip, args)| {
                    let (_, kind) = *SYSCALLS
                        .get(index)
                        .ok_or_else(|| io::Error::other("unknown seccomp event"))?;
                    let in_loader = loaders
                        .get(&pid)
                        .is_some_and(|range: &std::ops::Range<u64>| range.contains(&ip));
                    if !in_loader {
                        loaders.remove(&pid);
                    }
                    if sandboxed && matches!(kind, ExecAt | Exec) && exec_started {
                        trace.executable(None);
                        trace.denied = true;
                        kill_tracee(pid);
                        return Ok(Vec::new());
                    }
                    decode(
                        pid,
                        kind,
                        args,
                        pid == root && !exec_started,
                        in_loader,
                        &mut trace,
                    )
                });
                match decoded {
                    Ok(paths) if !paths.is_empty() => {
                        if sandboxed
                            && paths.iter().any(|access| {
                                access.kind == Mutate
                                    && !resolve(&access.path).starts_with(&rules.temp)
                            })
                        {
                            trace.denied = true;
                            trace.reason.get_or_insert("sandbox write outside TMPDIR");
                            kill_tracee(pid);
                        } else {
                            pending.insert(pid, paths);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        trace
                            .reason
                            .get_or_insert("could not read a syscall argument");
                    }
                }
            }
            c::PTRACE_EVENT_EXEC => {
                if let Some(range) = loader_text(pid) {
                    loaders.insert(pid, range);
                }
                if pid == root && !exec_started {
                    exec_started = true;
                } else {
                    trace.executable(fs::read_link(format!("/proc/{pid}/exe")).ok().as_deref());
                }
                if let Ok(old) = message(pid)
                    && old as c::pid_t != pid
                {
                    descendants.remove(&(old as c::pid_t));
                }
            }
            c::PTRACE_EVENT_CLONE | c::PTRACE_EVENT_FORK | c::PTRACE_EVENT_VFORK => {
                if let Ok(child) = message(pid) {
                    descendants.insert(child as c::pid_t);
                }
            }
            c::PTRACE_EVENT_STOP if signal != c::SIGTRAP => {
                let _ = ptrace(c::PTRACE_LISTEN, pid, std::ptr::null_mut());
                continue;
            }
            0 if signal == c::SIGTRAP | 0x80 => {
                if let Some(paths) = pending.remove(&pid) {
                    if let Ok(regs) = registers(pid) {
                        for access in paths {
                            trace.record_path(access, regs.rax as i64, &rules, sandboxed);
                        }
                    } else {
                        trace
                            .reason
                            .get_or_insert("could not read a syscall result");
                    }
                }
            }
            0 if sandboxed && signal == c::SIGSYS => {
                if let Ok(mut regs) = registers(pid)
                    && regs.orig_rax == c::SYS_socket as u64
                {
                    // RET_TRAP takes precedence over RET_TRACE. Return EACCES
                    // ourselves so a swallowed socket denial is still recorded.
                    trace.socket(regs.rdi as i32, None);
                    trace.denied = true;
                    regs.rax = (-i64::from(c::EACCES)) as u64;
                    if ptrace(
                        c::PTRACE_SETREGS,
                        pid,
                        (&mut regs as *mut c::user_regs_struct).cast(),
                    )
                    .is_err()
                    {
                        kill_tracee(pid);
                    }
                } else {
                    deliver = signal;
                }
            }
            0 => deliver = signal,
            _ => {}
        }
        let request = if pending.contains_key(&pid) {
            c::PTRACE_SYSCALL
        } else {
            c::PTRACE_CONT
        };
        let _ = ptrace(request, pid, deliver as usize as *mut _);
    }
}

fn is_terminfo(path: &Path) -> bool {
    [
        "/etc/terminfo",
        "/usr/share/terminfo",
        "/lib/terminfo",
        "/usr/lib/terminfo",
    ]
    .iter()
    .any(|base| path.starts_with(base))
}

fn kill_tracee(pid: c::pid_t) {
    // SAFETY: the positive PID is a live, stopped tracee.
    unsafe { c::kill(pid, c::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_records_paths_and_classifies_independently_of_log_budget() {
        let rules = Rules {
            checkout: "/fake-checkout".into(),
            temp: "/tmp".into(),
            binary: "/fake-checkout/bin".into(),
            libraries: vec![],
        };
        let mut trace = Trace::new();
        let read = |p: &str| Access {
            pid: 0,
            path: p.into(),
            kind: ReadContent,
            loader: false,
        };
        trace.record_path(
            read("/etc/leto-missing"),
            -i64::from(c::ENOENT),
            &rules,
            false,
        );
        assert_eq!(trace.evidence(true)["class"], "pure");
        trace.record_path(read("/fake-checkout/fixture"), 0, &rules, false);
        assert_eq!(trace.evidence(true)["class"], "files");
        trace.budget = 0;
        trace.record_path(
            Access {
                pid: 0,
                path: "/outside".into(),
                kind: Mutate,
                loader: false,
            },
            0,
            &rules,
            false,
        );
        assert_eq!(trace.evidence(true)["class"], "other");
        assert_eq!(trace.evidence(true)["truncated"], true);
        trace.executable(Some(Path::new("/usr/bin/true")));
        assert_eq!(trace.evidence(true)["class"], "exec");
        trace.socket(c::AF_INET, Some(true));
        assert_eq!(trace.evidence(true)["class"], "network");
    }

    #[test]
    fn shadow_mode_allows_readonly_terminfo_but_stage1_and_writes_stay_impure() {
        let rules = Rules {
            checkout: "/fake-checkout".into(),
            temp: "/tmp".into(),
            binary: "/fake-checkout/bin".into(),
            libraries: vec![],
        };
        let access = |p: &str, kind| Access {
            pid: 0,
            path: p.into(),
            kind,
            loader: false,
        };

        // Shadow mode: read-only terminfo lookups stay pure.
        let mut shadow = Trace::new();
        for path in [
            "/etc/terminfo",
            "/usr/share/terminfo",
            "/usr/share/terminfo/d/dumb",
            "/lib/terminfo/x/xterm-256color",
            "/usr/lib/terminfo/x/xterm",
        ] {
            shadow.record_path(access(path, ReadContent), 0, &rules, false);
        }
        let ev = shadow.evidence(true);
        assert_eq!(ev["class"], "pure");
        assert!(ev["reason"].is_null());

        // Shadow mode: writing to terminfo is still impure.
        shadow.record_path(
            access("/usr/share/terminfo/d/dumb", Mutate),
            0,
            &rules,
            false,
        );
        assert_eq!(shadow.evidence(true)["class"], "other");
        assert_eq!(shadow.evidence(true)["reason"], "write outside TMPDIR");

        // Stage 1 (sandboxed): terminfo reads remain impure.
        let mut stage1 = Trace::new();
        stage1.record_path(
            access("/usr/share/terminfo/d/dumb", ReadContent),
            0,
            &rules,
            true,
        );
        assert_eq!(stage1.evidence(true)["class"], "other");
    }
}
