use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

use serde_json::{Value, json};

use crate::observer::{self, Rules};

const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn relay(signal: libc::c_int) {
    let pid = CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe and the PID is positive. Preserve
        // the interrupted thread's errno across the call.
        unsafe {
            let errno = libc::__errno_location();
            let previous = *errno;
            libc::kill(pid, signal);
            *errno = previous;
        }
    }
}

struct Signals {
    old: Vec<(i32, libc::sigaction)>,
    set: libc::sigset_t,
}

impl Signals {
    fn new() -> io::Result<Self> {
        // SAFETY: these structures can be zero-initialized. The sigset and
        // previous actions are populated by libc before use.
        unsafe {
            let mut signals = Self {
                old: Vec::new(),
                set: std::mem::zeroed(),
            };
            libc::sigemptyset(&mut signals.set);
            for sig in SIGNALS {
                libc::sigaddset(&mut signals.set, sig);
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut previous) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if previous.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = relay as *const () as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(sig, &action, std::ptr::null_mut()) < 0 {
                    return Err(io::Error::last_os_error());
                }
                signals.old.push((sig, previous));
            }
            Ok(signals)
        }
    }

    fn fork(&self, test: bool) -> io::Result<libc::pid_t> {
        // SAFETY: the runner and monitor are single-threaded. The saved
        // mask and actions are initialized before use. No signal can relay
        // to the previous child during the fork.
        unsafe {
            let mut mask = std::mem::zeroed();
            if libc::sigprocmask(libc::SIG_BLOCK, &self.set, &mut mask) < 0 {
                return Err(io::Error::last_os_error());
            }
            let pid = libc::fork();
            let error = io::Error::last_os_error();
            CHILD.store(pid.max(0), Ordering::Relaxed);
            if pid == 0 && test {
                self.restore();
            }
            libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
            if pid < 0 { Err(error) } else { Ok(pid) }
        }
    }

    fn restore(&self) {
        for (sig, action) in &self.old {
            // SAFETY: each action was obtained from sigaction for this signal.
            unsafe { libc::sigaction(*sig, action, std::ptr::null_mut()) };
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        CHILD.store(0, Ordering::Relaxed);
        self.restore();
    }
}

pub fn exec(mut command: Command) -> ! {
    let error = command.exec();
    eprintln!("{:?}: {error}", command.get_program());
    std::process::exit(if error.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    });
}

pub fn wait(pid: libc::pid_t, options: i32) -> io::Result<(libc::pid_t, i32)> {
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes into a valid status buffer.
        let found = unsafe { libc::waitpid(pid, &mut status, options) };
        if found >= 0 {
            return Ok((found, status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub fn outcome(status: i32) -> Value {
    if libc::WIFSIGNALED(status) {
        json!({"signal": libc::WTERMSIG(status)})
    } else {
        json!({"exit": libc::WEXITSTATUS(status)})
    }
}

pub fn finish(status: i32) -> ! {
    if !libc::WIFSIGNALED(status) {
        std::process::exit(libc::WEXITSTATUS(status));
    }
    let sig = libc::WTERMSIG(status);
    // SAFETY: status came from waitpid, and the signal set is initialized
    // before use. SIGKILL cannot be caught or blocked.
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::signal(sig, libc::SIG_DFL);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(sig);
    }
    std::process::exit(128 + sig);
}

pub fn run(command: &mut Option<Command>, rules: Option<Rules>) -> io::Result<(i32, Value)> {
    let signals = Signals::new()?;
    let Some(rules) = rules else {
        let pid = signals.fork(true)?;
        if pid == 0 {
            exec(command.take().unwrap());
        }
        command.take();
        return Ok((wait(pid, 0)?.1, json!("unsampled")));
    };

    let (send, receive) = UnixStream::pair()?;
    let monitor = signals.fork(false)?;
    if monitor == 0 {
        drop(receive);
        observe(command.take().unwrap(), &signals, rules, send);
    }
    drop(send);
    let saved = command.take();
    let mut reader = BufReader::new(receive);
    let mut raw = [0; 4];
    if let Err(error) = reader.read_exact(&mut raw) {
        let status = wait(monitor, 0)?.1;
        if libc::WIFSIGNALED(status) {
            return Ok((status, json!("unavailable")));
        }
        return Err(error);
    }
    let status = i32::from_ne_bytes(raw);
    if status == -1 {
        *command = saved;
        let _ = wait(monitor, 0);
        return Err(io::Error::other("observer could not launch test"));
    }
    let mut response = String::new();
    let evidence = reader
        .read_line(&mut response)
        .ok()
        .and_then(|_| serde_json::from_str(&response).ok())
        .unwrap_or_else(|| json!("unavailable"));
    // The monitor may still be tracing a descendant. It has closed its test I/O.
    let _ = wait(monitor, libc::WNOHANG);
    Ok((status, evidence))
}

fn observe(command: Command, signals: &Signals, rules: Rules, report: UnixStream) -> ! {
    if let Ok((mut start, mut child)) = UnixStream::pair()
        && let Ok(pid) = signals.fork(true)
    {
        if pid == 0 {
            drop(report);
            drop(start);
            let mut requested = [0];
            let installed = child.read_exact(&mut requested).is_ok()
                && requested[0] != 0
                && observer::install().is_ok();
            // SAFETY: write uses a live fd and one initialized byte. It
            // avoids sendto, which the new filter would stop before the
            // monitor has consumed this acknowledgment.
            unsafe { libc::write(child.as_raw_fd(), [u8::from(installed)].as_ptr().cast(), 1) };
            drop(child);
            exec(command);
        }
        drop(child);
        // SAFETY: the monitor has no standard I/O after forking the test.
        // Closing these descriptors leaves the test's copies untouched.
        unsafe {
            libc::close(libc::STDIN_FILENO);
            libc::close(libc::STDOUT_FILENO);
            libc::close(libc::STDERR_FILENO);
        }
        let attached = observer::attach(pid).is_ok();
        let _ = start.write_all(&[u8::from(attached)]);
        let mut installed = [0];
        let filtered = start.read_exact(&mut installed).is_ok() && installed[0] != 0;
        drop(start);
        if attached {
            observer::collect(pid, rules, filtered, |status, evidence| {
                notify(report, status, evidence)
            });
        } else if let Ok((_, status)) = wait(pid, 0) {
            notify(report, status, json!("unavailable"));
        }
    } else {
        notify(report, -1, json!("unavailable"));
    }
    // SAFETY: no Rust cleanup in the forked monitor is required.
    unsafe { libc::_exit(0) }
}

fn notify(mut stream: UnixStream, status: i32, evidence: Value) {
    CHILD.store(0, Ordering::Relaxed);
    if stream.write_all(&status.to_ne_bytes()).is_ok()
        && let Ok(mut bytes) = serde_json::to_vec(&evidence)
    {
        bytes.push(b'\n');
        let _ = stream.write_all(&bytes);
    }
}
