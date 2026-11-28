mod control;
mod fuse_abi;
mod psi;
mod state;

use std::env;
use std::fs;
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::unix::fs::symlink;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::ptr;

use serde_json::json;

use crate::control::ControlServer;
use crate::fuse_abi::*;
use crate::psi::{PsiAction, PsiConfig, PsiGovernor};
use crate::state::{Effect, JobserverState, resolve_owner_metadata};

fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

struct ServeArgs {
    tokens: usize,
    mount_dir: PathBuf,
    path: Option<PathBuf>,
    socket: PathBuf,
    max_per_owner: usize,
    psi: PsiConfig,
}

fn usage() -> ! {
    eprintln!(
        "Usage:
  leto-jobserver serve [options]
    --tokens N                       Pool capacity (1..=4096, default: nproc)
    --mount-dir DIR                  FUSE3 mount directory (default: $XDG_RUNTIME_DIR/leto-jobserver)
    --path SYMLINK                   Optional symlink pointing to <mount-dir>/tokens
    --socket PATH                    Control/status Unix socket (default: $XDG_RUNTIME_DIR/leto-jobserver.sock)
    --max-per-owner N                Per-TGID token cap, 0=unlimited (default: 0)
    --psi-disabled                   Disable built-in PSI I/O throttling
    --psi-dir DIR                    PSI directory (default: /proc/pressure)
    --psi-interval-ms MS             PSI sample interval in ms (default: 1000)
    --psi-window N                   PSI moving-average window (default: 3)
    --psi-high F                     PSI io full avg10 withhold threshold (default: 20.0)
    --psi-low F                      PSI io full avg10 release threshold (default: 8.0)
    --psi-step N                     Tokens withheld/released per tick (default: 4)
    --psi-max-withhold-fraction F    Max fraction of capacity withheld (default: 0.75)

  leto-jobserver status [--socket PATH] [--json]
  leto-jobserver resize N [--socket PATH]
  leto-jobserver set-cap N [--socket PATH]"
    );
    std::process::exit(2);
}

fn next_arg(args: &[String], i: &mut usize, flag: &str) -> String {
    *i += 1;
    args.get(*i).cloned().unwrap_or_else(|| {
        eprintln!("leto-jobserver: missing value for {flag}");
        std::process::exit(2);
    })
}

fn parse_serve(args: &[String]) -> ServeArgs {
    let rt = runtime_dir();
    let default_tokens = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let mut cfg = ServeArgs {
        tokens: default_tokens,
        mount_dir: rt.join("leto-jobserver"),
        path: None,
        socket: rt.join("leto-jobserver.sock"),
        max_per_owner: 0,
        psi: PsiConfig::default(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" => {
                cfg.tokens = next_arg(args, &mut i, "--tokens")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--mount-dir" => cfg.mount_dir = PathBuf::from(next_arg(args, &mut i, "--mount-dir")),
            "--path" => cfg.path = Some(PathBuf::from(next_arg(args, &mut i, "--path"))),
            "--socket" => cfg.socket = PathBuf::from(next_arg(args, &mut i, "--socket")),
            "--max-per-owner" => {
                cfg.max_per_owner = next_arg(args, &mut i, "--max-per-owner")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--psi-disabled" => cfg.psi.enabled = false,
            "--psi-dir" => cfg.psi.psi_dir = PathBuf::from(next_arg(args, &mut i, "--psi-dir")),
            "--psi-interval-ms" => {
                cfg.psi.interval_ms = next_arg(args, &mut i, "--psi-interval-ms")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--psi-window" => {
                cfg.psi.window = next_arg(args, &mut i, "--psi-window")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--psi-high" => {
                cfg.psi.high = next_arg(args, &mut i, "--psi-high")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--psi-low" => {
                cfg.psi.low = next_arg(args, &mut i, "--psi-low")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--psi-step" => {
                cfg.psi.step = next_arg(args, &mut i, "--psi-step")
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--psi-max-withhold-fraction" => {
                cfg.psi.max_withhold_fraction =
                    next_arg(args, &mut i, "--psi-max-withhold-fraction")
                        .parse()
                        .unwrap_or_else(|_| usage());
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("leto-jobserver: unknown option {other}");
                usage();
            }
        }
        i += 1;
    }
    if !(1..=4096).contains(&cfg.tokens) {
        eprintln!("leto-jobserver: --tokens must be between 1 and 4096");
        std::process::exit(2);
    }
    if cfg.max_per_owner > 4096 {
        eprintln!("leto-jobserver: --max-per-owner must be between 0 and 4096");
        std::process::exit(2);
    }
    if let Err(err) = cfg.psi.validate() {
        eprintln!("leto-jobserver: {err}");
        std::process::exit(2);
    }
    cfg
}

fn setup_signalfd() -> io::Result<OwnedFd> {
    unsafe {
        let mut mask: libc::sigset_t = MaybeUninit::zeroed().assume_init();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigaddset(&mut mask, libc::SIGINT);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &mask, ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::signalfd(-1, &mask, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn setup_timerfd(interval_ms: u64) -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let secs = (interval_ms / 1000) as libc::time_t;
        let nsecs = ((interval_ms % 1000) * 1_000_000) as libc::c_long;
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: secs,
                tv_nsec: nsecs,
            },
            it_value: libc::timespec {
                tv_sec: secs,
                tv_nsec: nsecs,
            },
        };
        if libc::timerfd_settime(fd, 0, &spec, ptr::null_mut()) < 0 {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn install_symlink(link: &Path, target: &Path) -> io::Result<()> {
    if let Ok(meta) = fs::symlink_metadata(link)
        && meta.file_type().is_dir()
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} is a directory", link.display()),
        ));
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = link.with_file_name(format!(
        ".{}.tmp.{}",
        link.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("build-jobserver"),
        std::process::id()
    ));
    let _ = fs::remove_file(&tmp);
    symlink(target, &tmp)?;
    fs::rename(&tmp, link)
}

fn cleanup_symlink(link: &Path, expected_target: &Path) {
    if let Ok(cur) = fs::read_link(link)
        && cur == expected_target
    {
        let _ = fs::remove_file(link);
    }
}

fn apply_effects(fuse_fd: RawFd, effects: Vec<Effect>) {
    for eff in effects {
        match eff {
            Effect::ReplyByte { unique } => write_reply(fuse_fd, unique, 0, b"+"),
            Effect::ReplyError { unique, errno } => write_reply(fuse_fd, unique, errno, &[]),
            Effect::NotifyPoll { kh } => notify_poll(fuse_fd, kh),
        }
    }
}

fn dir_attr(uid: u32, gid: u32) -> FuseAttr {
    FuseAttr {
        ino: FUSE_ROOT_ID,
        size: 0,
        mode: libc::S_IFDIR | 0o700,
        nlink: 2,
        uid,
        gid,
        blksize: 4096,
        ..Default::default()
    }
}

fn file_attr(uid: u32, gid: u32) -> FuseAttr {
    FuseAttr {
        ino: TOKENS_INO,
        size: 0,
        mode: libc::S_IFREG | 0o600,
        nlink: 1,
        uid,
        gid,
        blksize: 4096,
        ..Default::default()
    }
}

fn handle_fuse_request(
    fuse_fd: RawFd,
    uid: u32,
    gid: u32,
    state: &mut JobserverState,
    hdr: FuseInHeader,
    body: &[u8],
) -> bool {
    match hdr.opcode {
        FUSE_INIT => {
            let init_in = read_struct::<FuseInitIn>(body).unwrap_or_default();
            let wanted = FUSE_ASYNC_READ | FUSE_PARALLEL_DIROPS;
            let init_out = FuseInitOut {
                major: FUSE_KERNEL_VERSION,
                minor: init_in.minor.min(FUSE_KERNEL_MINOR_VERSION),
                max_readahead: 0,
                flags: init_in.flags & wanted,
                max_background: 64,
                congestion_threshold: 48,
                max_write: 4096,
                time_gran: 1,
                ..Default::default()
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&init_out));
        }
        FUSE_LOOKUP => {
            let name = body.split(|&b| b == 0).next().unwrap_or(&[]);
            if hdr.nodeid == FUSE_ROOT_ID && name == b"tokens" {
                let out = FuseEntryOut {
                    nodeid: TOKENS_INO,
                    generation: 1,
                    entry_valid: 3600,
                    attr_valid: 3600,
                    attr: file_attr(uid, gid),
                    ..Default::default()
                };
                write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
            } else {
                write_reply(fuse_fd, hdr.unique, -libc::ENOENT, &[]);
            }
        }
        FUSE_GETATTR => {
            let attr = match hdr.nodeid {
                FUSE_ROOT_ID => dir_attr(uid, gid),
                TOKENS_INO => file_attr(uid, gid),
                _ => {
                    write_reply(fuse_fd, hdr.unique, -libc::ENOENT, &[]);
                    return false;
                }
            };
            let out = FuseAttrOut {
                attr_valid: 3600,
                attr,
                ..Default::default()
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
        }
        FUSE_SETATTR => {
            let attr = if hdr.nodeid == TOKENS_INO {
                file_attr(uid, gid)
            } else {
                dir_attr(uid, gid)
            };
            let out = FuseAttrOut {
                attr_valid: 0,
                attr,
                ..Default::default()
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
        }
        FUSE_ACCESS => write_reply(fuse_fd, hdr.unique, 0, &[]),
        FUSE_STATFS => {
            let st = FuseKstatfs {
                bsize: 4096,
                namelen: 255,
                frsize: 4096,
                ..Default::default()
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&st));
        }
        FUSE_OPENDIR => {
            let out = FuseOpenOut::default();
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
        }
        FUSE_READDIR => {
            let read_in = read_struct::<FuseReadIn>(body).unwrap_or_default();
            let entries: &[(u64, u32, &[u8])] = &[
                (FUSE_ROOT_ID, libc::DT_DIR as u32, b"."),
                (FUSE_ROOT_ID, libc::DT_DIR as u32, b".."),
                (TOKENS_INO, libc::DT_REG as u32, b"tokens"),
            ];
            let mut buf = Vec::new();
            for (idx, &(ino, typ, name)) in entries.iter().enumerate() {
                let off = (idx + 1) as u64;
                if off <= read_in.offset {
                    continue;
                }
                let dirent = FuseDirent {
                    ino,
                    off,
                    namelen: name.len() as u32,
                    typ,
                };
                let rec_len = (mem::size_of::<FuseDirent>() + name.len() + 7) & !7;
                if buf.len() + rec_len > read_in.size as usize {
                    break;
                }
                buf.extend_from_slice(as_bytes(&dirent));
                buf.extend_from_slice(name);
                buf.resize(
                    buf.len() + (rec_len - mem::size_of::<FuseDirent>() - name.len()),
                    0,
                );
            }
            write_reply(fuse_fd, hdr.unique, 0, &buf);
        }
        FUSE_RELEASEDIR => write_reply(fuse_fd, hdr.unique, 0, &[]),
        FUSE_OPEN => {
            if hdr.nodeid != TOKENS_INO {
                write_reply(fuse_fd, hdr.unique, -libc::EISDIR, &[]);
                return false;
            }
            let _open_in = read_struct::<FuseOpenIn>(body).unwrap_or_default();
            let (tgid, comm, cmdline) = resolve_owner_metadata(hdr.pid);
            let fh = state.on_open(tgid, comm, cmdline);
            let out = FuseOpenOut {
                fh,
                open_flags: FOPEN_DIRECT_IO
                    | FOPEN_NONSEEKABLE
                    | FOPEN_STREAM
                    | FOPEN_PARALLEL_DIRECT_WRITES,
                backing_id: 0,
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
        }
        FUSE_READ => {
            let Some(read_in) = read_struct::<FuseReadIn>(body) else {
                write_reply(fuse_fd, hdr.unique, -libc::EINVAL, &[]);
                return false;
            };
            let nonblock = (read_in.flags & (libc::O_NONBLOCK as u32)) != 0;
            if let Some(eff) = state.on_read(hdr.unique, read_in.fh, read_in.size, nonblock) {
                apply_effects(fuse_fd, vec![eff]);
            }
        }
        FUSE_WRITE => {
            let Some(write_in) = read_struct::<FuseWriteIn>(body) else {
                write_reply(fuse_fd, hdr.unique, -libc::EINVAL, &[]);
                return false;
            };
            let (written, effects) = state.on_write(write_in.fh, write_in.size as usize);
            let out = FuseWriteOut {
                size: written as u32,
                padding: 0,
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
            apply_effects(fuse_fd, effects);
        }
        FUSE_POLL => {
            let Some(poll_in) = read_struct::<FusePollIn>(body) else {
                write_reply(fuse_fd, hdr.unique, -libc::EINVAL, &[]);
                return false;
            };
            let schedule = (poll_in.flags & FUSE_POLL_SCHEDULE_NOTIFY) != 0;
            let revents = state.on_poll(poll_in.fh, poll_in.kh, schedule, poll_in.events);
            let out = FusePollOut {
                revents,
                padding: 0,
            };
            write_reply(fuse_fd, hdr.unique, 0, as_bytes(&out));
        }
        FUSE_INTERRUPT => {
            if let Some(int_in) = read_struct::<FuseInterruptIn>(body)
                && let Some(eff) = state.on_interrupt(int_in.unique)
            {
                apply_effects(fuse_fd, vec![eff]);
            }
        }
        FUSE_FLUSH => write_reply(fuse_fd, hdr.unique, 0, &[]),
        FUSE_RELEASE => {
            let effects = if let Some(rel_in) = read_struct::<FuseReleaseIn>(body) {
                let (reclaimed, info, effects) = state.on_release(rel_in.fh);
                if reclaimed > 0
                    && let Some((tgid, comm)) = info
                {
                    eprintln!(
                        "leto-jobserver: reclaimed {reclaimed} token(s) from exited tgid={tgid} comm={comm} (available={}/{})",
                        state.available(),
                        state.effective_capacity()
                    );
                }
                effects
            } else {
                Vec::new()
            };
            write_reply(fuse_fd, hdr.unique, 0, &[]);
            apply_effects(fuse_fd, effects);
        }
        FUSE_FORGET | FUSE_BATCH_FORGET => {}
        FUSE_LSEEK => write_reply(fuse_fd, hdr.unique, -libc::ESPIPE, &[]),
        FUSE_DESTROY => {
            write_reply(fuse_fd, hdr.unique, 0, &[]);
            return true;
        }
        _ => write_reply(fuse_fd, hdr.unique, -libc::ENOSYS, &[]),
    }
    false
}

fn run_serve(cfg: ServeArgs) -> io::Result<()> {
    cleanup_stale_mount(&cfg.mount_dir);
    fs::create_dir_all(&cfg.mount_dir)?;
    let sig_fd = setup_signalfd()?;
    let timer_fd = if cfg.psi.enabled {
        Some(setup_timerfd(cfg.psi.interval_ms)?)
    } else {
        None
    };
    let fuse_fd = mount_fusermount3(&cfg.mount_dir)?;
    let tokens_target = cfg.mount_dir.join("tokens");
    if let Some(link) = &cfg.path
        && let Err(err) = install_symlink(link, &tokens_target)
    {
        drop(fuse_fd);
        unmount_fusermount3(&cfg.mount_dir);
        return Err(err);
    }
    let mut ctl = match ControlServer::bind(cfg.socket.clone()) {
        Ok(c) => c,
        Err(err) => {
            if let Some(link) = &cfg.path {
                cleanup_symlink(link, &tokens_target);
            }
            drop(fuse_fd);
            unmount_fusermount3(&cfg.mount_dir);
            return Err(err);
        }
    };

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let psi_enabled = cfg.psi.enabled;
    let mut state = JobserverState::new(
        cfg.tokens,
        cfg.max_per_owner,
        PsiGovernor::new(cfg.psi),
        cfg.mount_dir.display().to_string(),
        cfg.path
            .as_ref()
            .map(|p| p.display().to_string())
            .or_else(|| Some(tokens_target.display().to_string())),
    );
    eprintln!(
        "leto-jobserver: ready tokens={} max_per_owner={} psi={} mount={} socket={}",
        cfg.tokens,
        cfg.max_per_owner,
        psi_enabled,
        cfg.mount_dir.display(),
        cfg.socket.display()
    );

    let fuse_raw = fuse_fd.as_raw_fd();
    let sig_raw = sig_fd.as_raw_fd();
    let ctl_raw = ctl.listener_fd();
    let timer_raw = timer_fd.as_ref().map(|f| f.as_raw_fd());
    let mut buf = vec![0u8; 65536];
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(32);

    'event_loop: loop {
        pollfds.clear();
        pollfds.push(libc::pollfd {
            fd: fuse_raw,
            events: libc::POLLIN,
            revents: 0,
        });
        pollfds.push(libc::pollfd {
            fd: sig_raw,
            events: libc::POLLIN,
            revents: 0,
        });
        pollfds.push(libc::pollfd {
            fd: ctl_raw,
            events: libc::POLLIN,
            revents: 0,
        });
        if let Some(tfd) = timer_raw {
            pollfds.push(libc::pollfd {
                fd: tfd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let client_start = pollfds.len();
        for (cfd, events) in ctl.client_pollfds() {
            pollfds.push(libc::pollfd {
                fd: cfd,
                events,
                revents: 0,
            });
        }

        let timeout_ms = if pollfds.len() > client_start {
            1000
        } else {
            -1
        };
        let rc = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!("leto-jobserver: poll error: {err}");
            break;
        }

        if (pollfds[1].revents & libc::POLLIN) != 0 {
            let mut info: libc::signalfd_siginfo = unsafe { MaybeUninit::zeroed().assume_init() };
            let _ = unsafe {
                libc::read(
                    sig_raw,
                    (&mut info as *mut libc::signalfd_siginfo).cast(),
                    mem::size_of::<libc::signalfd_siginfo>(),
                )
            };
            break 'event_loop;
        }

        if (pollfds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP)) != 0 {
            let n = unsafe { libc::read(fuse_raw, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EINTR) | Some(libc::ENOENT) => {}
                    Some(libc::ENODEV) => break 'event_loop,
                    _ => {
                        eprintln!("leto-jobserver: read(/dev/fuse) error: {err}");
                        break 'event_loop;
                    }
                }
            } else if (n as usize) >= mem::size_of::<FuseInHeader>() {
                let n = n as usize;
                let hdr = read_struct::<FuseInHeader>(&buf[..n]).unwrap();
                let body = &buf[mem::size_of::<FuseInHeader>()..n];
                if handle_fuse_request(fuse_raw, uid, gid, &mut state, hdr, body) {
                    break 'event_loop;
                }
            }
        }

        if (pollfds[2].revents & libc::POLLIN) != 0 {
            ctl.accept_ready();
        }

        if let Some(tfd) = timer_raw
            && (pollfds[3].revents & libc::POLLIN) != 0
        {
            let mut expirations: u64 = 0;
            let _ = unsafe { libc::read(tfd, (&mut expirations as *mut u64).cast(), 8) };
            match state.tick_psi() {
                Ok((action, effects)) => {
                    match action {
                        PsiAction::Withheld {
                            delta,
                            new_withheld,
                        } => {
                            let snap = state.status();
                            eprintln!(
                                "leto-jobserver: PSI withheld {delta} token(s) (withheld={new_withheld}, effective={}/{}, io={:.2}, smoothed={:.2}, mem={:.2})",
                                snap.effective_capacity,
                                snap.capacity,
                                snap.psi.io_full_avg10.unwrap_or(-1.0),
                                snap.psi.io_smoothed.unwrap_or(-1.0),
                                snap.psi.memory_some_avg10.unwrap_or(-1.0),
                            );
                        }
                        PsiAction::Released {
                            delta,
                            new_withheld,
                        } => {
                            let snap = state.status();
                            eprintln!(
                                "leto-jobserver: PSI released {delta} token(s) (withheld={new_withheld}, effective={}/{}, io={:.2}, smoothed={:.2}, mem={:.2})",
                                snap.effective_capacity,
                                snap.capacity,
                                snap.psi.io_full_avg10.unwrap_or(-1.0),
                                snap.psi.io_smoothed.unwrap_or(-1.0),
                                snap.psi.memory_some_avg10.unwrap_or(-1.0),
                            );
                        }
                        PsiAction::Unchanged => {}
                    }
                    apply_effects(fuse_raw, effects);
                }
                Err(err) => {
                    eprintln!("leto-jobserver: PSI sample error: {err}");
                }
            }
        }

        let ready_clients: Vec<(RawFd, i16)> = pollfds[client_start..]
            .iter()
            .filter(|p| p.revents != 0)
            .map(|p| (p.fd, p.revents))
            .collect();
        for (cfd, revents) in ready_clients {
            let effects = ctl.handle_client_fd(cfd, revents, &mut state);
            apply_effects(fuse_raw, effects);
        }
        ctl.expire_idle();
    }

    ctl.cleanup();
    if let Some(link) = &cfg.path {
        cleanup_symlink(link, &tokens_target);
    }
    drop(fuse_fd);
    unmount_fusermount3(&cfg.mount_dir);
    Ok(())
}

fn parse_client_socket(args: &[String]) -> (PathBuf, bool) {
    let mut socket = runtime_dir().join("leto-jobserver.sock");
    let mut as_json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => socket = PathBuf::from(next_arg(args, &mut i, "--socket")),
            "--json" => as_json = true,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("leto-jobserver: unknown option {other}");
                usage();
            }
        }
        i += 1;
    }
    (socket, as_json)
}

fn print_human_status(v: &serde_json::Value) {
    println!(
        "leto-jobserver pid={} uptime={}s capacity={} withheld={} effective={} held={} available={} max_per_owner={} waiters={} pollers={} reclaimed={}",
        v["pid"],
        v["uptime_secs"],
        v["capacity"],
        v["withheld"],
        v["effective_capacity"],
        v["held"],
        v["available"],
        v["max_per_owner"],
        v["waiters"],
        v["pollers"],
        v["reclaimed_total"],
    );
    if v["psi"]["enabled"].as_bool() == Some(true) {
        println!(
            "  psi: io_full_avg10={} io_smoothed={} memory_some_avg10={} high={} low={} cap={}",
            v["psi"]["io_full_avg10"],
            v["psi"]["io_smoothed"],
            v["psi"]["memory_some_avg10"],
            v["psi"]["high"],
            v["psi"]["low"],
            v["psi"]["withhold_cap"],
        );
    }
    if let Some(holders) = v["holders"].as_array() {
        for h in holders {
            println!(
                "  holder tgid={} comm={} held={} fhs={} cmd={}",
                h["tgid"],
                h["comm"].as_str().unwrap_or(""),
                h["held"],
                h["open_fhs"],
                h["cmdline"].as_str().unwrap_or(""),
            );
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let res = match args[1].as_str() {
        "serve" => run_serve(parse_serve(&args[2..])),
        "status" => {
            let (socket, as_json) = parse_client_socket(&args[2..]);
            control::send_request(&socket, &json!({ "cmd": "status" })).map(|v| {
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap());
                } else {
                    print_human_status(&v);
                }
            })
        }
        "resize" => {
            if args.len() < 3 {
                usage();
            }
            let capacity: usize = args[2].parse().unwrap_or_else(|_| usage());
            let (socket, _) = parse_client_socket(&args[3..]);
            control::send_request(&socket, &json!({ "cmd": "resize", "capacity": capacity })).map(
                |v| {
                    if v["ok"].as_bool() != Some(true) {
                        eprintln!("leto-jobserver: {}", v["error"]);
                        std::process::exit(1);
                    }
                    print_human_status(&v);
                },
            )
        }
        "set-cap" => {
            if args.len() < 3 {
                usage();
            }
            let cap: usize = args[2].parse().unwrap_or_else(|_| usage());
            let (socket, _) = parse_client_socket(&args[3..]);
            control::send_request(
                &socket,
                &json!({ "cmd": "set_max_per_owner", "max_per_owner": cap }),
            )
            .map(|v| {
                if v["ok"].as_bool() != Some(true) {
                    eprintln!("leto-jobserver: {}", v["error"]);
                    std::process::exit(1);
                }
                print_human_status(&v);
            })
        }
        _ => usage(),
    };
    if let Err(err) = res {
        eprintln!("leto-jobserver: {err}");
        std::process::exit(1);
    }
}
