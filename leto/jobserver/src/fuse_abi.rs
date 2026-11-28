use std::io;
use std::mem::{self, MaybeUninit};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::ptr;

pub const FUSE_KERNEL_VERSION: u32 = 7;
// 7.31 adds FOPEN_STREAM; 7.36 adds FOPEN_PARALLEL_DIRECT_WRITES.
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 38;
pub const FUSE_ROOT_ID: u64 = 1;
pub const TOKENS_INO: u64 = 2;

pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2;
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_INTERRUPT: u32 = 36;
pub const FUSE_DESTROY: u32 = 38;
pub const FUSE_POLL: u32 = 40;
pub const FUSE_BATCH_FORGET: u32 = 42;
pub const FUSE_LSEEK: u32 = 46;

pub const FUSE_NOTIFY_POLL: i32 = 1;

pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
pub const FOPEN_NONSEEKABLE: u32 = 1 << 2;
pub const FOPEN_STREAM: u32 = 1 << 4;
pub const FOPEN_PARALLEL_DIRECT_WRITES: u32 = 1 << 6;

pub const FUSE_ASYNC_READ: u32 = 1 << 0;
pub const FUSE_PARALLEL_DIROPS: u32 = 1 << 18;

pub const FUSE_POLL_SCHEDULE_NOTIFY: u32 = 1 << 0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub total_extlen: u16,
    pub padding: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseEntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseAttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseOpenIn {
    pub flags: u32,
    pub open_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseOpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub backing_id: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseReleaseIn {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseWriteIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseWriteOut {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub flags2: u32,
    pub unused: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub flags2: u32,
    pub max_stack_depth: u32,
    pub request_timeout: u16,
    pub unused: [u16; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseInterruptIn {
    pub unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FusePollIn {
    pub fh: u64,
    pub kh: u64,
    pub flags: u32,
    pub events: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FusePollOut {
    pub revents: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseNotifyPollWakeupOut {
    pub kh: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseDirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub typ: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FuseKstatfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

pub fn read_struct<T: Copy>(buf: &[u8]) -> Option<T> {
    if buf.len() < mem::size_of::<T>() {
        return None;
    }
    // SAFETY: length checked above; `read_unaligned` handles arbitrary alignment.
    Some(unsafe { ptr::read_unaligned(buf.as_ptr().cast::<T>()) })
}

pub fn as_bytes<T>(val: &T) -> &[u8] {
    // SAFETY: all FUSE wire structs are `#[repr(C)]` with explicit padding and
    // initialized fields, and the returned slice does not outlive `val`.
    unsafe { std::slice::from_raw_parts((val as *const T).cast::<u8>(), mem::size_of::<T>()) }
}

/// Writes a FUSE reply to `/dev/fuse`. Returns `Err(errno)` on failure (such as
/// `ENOENT` when the kernel already aborted or interrupted the request).
pub fn write_reply(fuse_fd: RawFd, unique: u64, error: i32, payload: &[u8]) -> Result<(), i32> {
    let hdr = FuseOutHeader {
        len: (mem::size_of::<FuseOutHeader>() + payload.len()) as u32,
        error,
        unique,
    };
    let iovs = [
        libc::iovec {
            iov_base: (&hdr as *const FuseOutHeader).cast_mut().cast(),
            iov_len: mem::size_of::<FuseOutHeader>(),
        },
        libc::iovec {
            iov_base: payload.as_ptr().cast_mut().cast(),
            iov_len: payload.len(),
        },
    ];
    let n = if payload.is_empty() { 1 } else { 2 };
    let ret = unsafe { libc::writev(fuse_fd, iovs.as_ptr(), n) };
    if ret < 0 {
        let errno = io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        return Err(errno);
    }
    Ok(())
}

pub fn notify_poll(fuse_fd: RawFd, kh: u64) -> Result<(), i32> {
    let body = FuseNotifyPollWakeupOut { kh };
    write_reply(fuse_fd, 0, FUSE_NOTIFY_POLL, as_bytes(&body))
}

fn find_fusermount3() -> &'static str {
    if Path::new("/usr/bin/fusermount3").exists() {
        "/usr/bin/fusermount3"
    } else {
        "fusermount3"
    }
}

/// Clears a stale FUSE mountpoint left behind by a SIGKILLed previous daemon.
pub fn cleanup_stale_mount(mountpoint: &Path) {
    match std::fs::metadata(mountpoint) {
        Err(err) if err.raw_os_error() == Some(libc::ENOTCONN) => {
            unmount_fusermount3(mountpoint);
        }
        _ => {}
    }
}

#[repr(C, align(8))]
struct CmsgBuf([u8; 128]);

pub fn mount_fusermount3(mountpoint: &Path) -> io::Result<OwnedFd> {
    let mut sv = [0 as RawFd; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let parent_fd = unsafe { OwnedFd::from_raw_fd(sv[0]) };
    let child_fd = unsafe { OwnedFd::from_raw_fd(sv[1]) };
    // Clear CLOEXEC only on the child end passed via _FUSE_COMMFD.
    unsafe {
        libc::fcntl(child_fd.as_raw_fd(), libc::F_SETFD, 0);
    }

    let mut child = Command::new(find_fusermount3())
        .arg("-o")
        .arg("fsname=leto-jobserver,subtype=jobserver,noatime,nodev,nosuid")
        .arg("--")
        .arg(mountpoint)
        .env("_FUSE_COMMFD", child_fd.as_raw_fd().to_string())
        .stdin(Stdio::null())
        .spawn()?;
    drop(child_fd);

    let mut dummy = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: dummy.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut cmsg_buf = CmsgBuf([0u8; 128]);
    let mut msg: libc::msghdr = unsafe { MaybeUninit::zeroed().assume_init() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.0.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_buf.0.len();

    let n = unsafe { libc::recvmsg(parent_fd.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    let recv_err = if n < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    let status = child.wait()?;
    if let Some(err) = recv_err {
        return Err(err);
    }
    if !status.success() {
        return Err(io::Error::other(format!("fusermount3 exited {status}")));
    }
    let truncated = (msg.msg_flags & libc::MSG_CTRUNC) != 0;
    let mut received: Option<RawFd> = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_len =
                    ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let count = data_len / mem::size_of::<RawFd>();
                let base = libc::CMSG_DATA(cmsg);
                for i in 0..count {
                    let fd =
                        ptr::read_unaligned(base.add(i * mem::size_of::<RawFd>()).cast::<RawFd>());
                    if !truncated && received.is_none() {
                        received = Some(fd);
                    } else {
                        libc::close(fd);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if truncated {
        return Err(io::Error::other("fusermount3 control message truncated"));
    }
    let fd = received.ok_or_else(|| io::Error::other("fusermount3 sent no /dev/fuse fd"))?;
    // Set O_NONBLOCK so a cancelled FUSE request between poll() and read() can
    // never block the single-threaded event loop.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn unmount_fusermount3(mountpoint: &Path) {
    let bin = find_fusermount3();
    let ok = Command::new(bin)
        .arg("-u")
        .arg(mountpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = Command::new(bin)
            .arg("-u")
            .arg("-z")
            .arg(mountpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
