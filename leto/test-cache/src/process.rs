use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use serde_json::{Value, json};

use crate::observer::{self, Rules};
use crate::sandbox;

const SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];
static CHILD: AtomicI32 = AtomicI32::new(0);
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
const LAUNCH_FAILED: i32 = -1;
const RETRY_UNFILTERED: i32 = -2;
const SANDBOX_UNAVAILABLE: i32 = -3;
const FILTER_INSTALLED: u8 = 1;
const FILTER_UNAVAILABLE: u8 = 2;

extern "C" fn relay(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::Relaxed);
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

pub fn interrupted() -> Option<i32> {
    let signal = INTERRUPTED.load(Ordering::Relaxed);
    (signal != 0).then_some(signal)
}

pub struct Capture {
    stdout: File,
    stderr: File,
}

impl Capture {
    pub fn new() -> io::Result<Self> {
        fn stream(name: &std::ffi::CStr) -> io::Result<File> {
            // SAFETY: the kernel reads the NUL-terminated name and returns a fresh fd.
            let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the successful syscall returned a new, owned fd.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
        Ok(Self {
            stdout: stream(c"leto-test-stdout")?,
            stderr: stream(c"leto-test-stderr")?,
        })
    }

    fn writer(file: &File) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .open(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }

    pub fn redirect(&self, stdin: &File) -> io::Result<()> {
        let stdout = Self::writer(&self.stdout)?;
        let stderr = Self::writer(&self.stderr)?;
        for (file, fd) in [(stdin, 0), (&stdout, 1), (&stderr, 2)] {
            // SAFETY: every source descriptor is live; the child owns the stdio table.
            if unsafe { libc::dup2(file.as_raw_fd(), fd) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn command(&self, command: &mut Command) -> io::Result<()> {
        command
            .stdout(Stdio::from(Self::writer(&self.stdout)?))
            .stderr(Stdio::from(Self::writer(&self.stderr)?));
        Ok(())
    }

    pub fn output(&self, limit: usize) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let stdout = self.stdout.metadata()?.len();
        let stderr = self.stderr.metadata()?.len();
        if stdout.saturating_add(stderr) > limit as u64 {
            return Ok(None);
        }
        let mut streams = (vec![0; stdout as usize], vec![0; stderr as usize]);
        self.stdout.read_exact_at(&mut streams.0, 0)?;
        self.stderr.read_exact_at(&mut streams.1, 0)?;
        Ok(Some(streams))
    }

    pub fn emit(mut self) -> io::Result<()> {
        self.stdout.rewind()?;
        self.stderr.rewind()?;
        io::copy(&mut self.stdout, &mut io::stdout().lock())?;
        io::copy(&mut self.stderr, &mut io::stderr().lock())?;
        Ok(())
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

fn run_plain(
    signals: &Signals,
    command: &mut Option<Command>,
    evidence: &str,
) -> io::Result<(i32, Value)> {
    let pid = signals.fork(true)?;
    if pid == 0 {
        exec(command.take().unwrap());
    }
    command.take();
    Ok((wait(pid, 0)?.1, json!(evidence)))
}

pub fn run(command: &mut Option<Command>, rules: Option<Rules>) -> io::Result<(i32, Value)> {
    let signals = Signals::new()?;
    let Some(rules) = rules else {
        return run_plain(&signals, command, "unsampled");
    };
    run_observed(command, &signals, rules, None)?
        .ok_or_else(|| io::Error::other("observer unavailable"))
}

pub fn run_sandbox(
    command: &mut Option<Command>,
    rules: Rules,
    launch: &sandbox::Launch<'_>,
) -> io::Result<Option<(i32, Value)>> {
    run_observed(command, &Signals::new()?, rules, Some(launch))
}

fn run_observed(
    command: &mut Option<Command>,
    signals: &Signals,
    rules: Rules,
    launch: Option<&sandbox::Launch<'_>>,
) -> io::Result<Option<(i32, Value)>> {
    let (send, receive) = UnixStream::pair()?;
    let monitor = signals.fork(false)?;
    if monitor == 0 {
        drop(receive);
        observe(command.take().unwrap(), signals, rules, send, launch);
    }
    drop(send);
    let saved = command.take();
    let mut reader = BufReader::new(receive);
    let mut raw = [0; 4];
    if let Err(error) = reader.read_exact(&mut raw) {
        let status = wait(monitor, 0)?.1;
        if libc::WIFSIGNALED(status) {
            return Ok(Some((status, json!("unavailable"))));
        }
        return Err(error);
    }
    let status = i32::from_ne_bytes(raw);
    if matches!(
        status,
        LAUNCH_FAILED | RETRY_UNFILTERED | SANDBOX_UNAVAILABLE
    ) {
        *command = saved;
        let _ = wait(monitor, 0);
        if status == RETRY_UNFILTERED {
            return run_plain(signals, command, "unavailable").map(Some);
        }
        if status == SANDBOX_UNAVAILABLE {
            return Ok(None);
        }
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
    Ok(Some((status, evidence)))
}

fn observe(
    command: Command,
    signals: &Signals,
    rules: Rules,
    report: UnixStream,
    launch: Option<&sandbox::Launch<'_>>,
) -> ! {
    if let Ok((mut start, mut child)) = UnixStream::pair()
        && let Ok(pid) = signals.fork(true)
    {
        if pid == 0 {
            drop(report);
            drop(start);
            let mut requested = [0];
            let attached = child.read_exact(&mut requested).is_ok() && requested[0] != 0;
            let mode = match launch {
                Some(launch) if attached => {
                    match launch.prepare(&rules).and_then(|()| observer::install()) {
                        Ok(()) => FILTER_INSTALLED,
                        Err(_) => FILTER_UNAVAILABLE,
                    }
                }
                Some(_) => FILTER_UNAVAILABLE,
                None if !attached => 0,
                None if observer::install().is_ok() => FILTER_INSTALLED,
                None => FILTER_UNAVAILABLE,
            };
            // SAFETY: write uses a live fd and one initialized byte. It
            // avoids sendto, which the new filter would stop before the
            // monitor has consumed this acknowledgment.
            unsafe { libc::write(child.as_raw_fd(), [mode].as_ptr().cast(), 1) };
            if mode == FILTER_UNAVAILABLE {
                // SAFETY: the launcher can exit directly. The test has not started.
                unsafe { libc::_exit(0) };
            }
            drop(child);
            if let Some(launch) = launch {
                launch.exec();
            }
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
        let attached = observer::attach(pid, launch.is_some()).is_ok();
        let _ = start.write_all(&[u8::from(attached)]);
        let mut mode = [0];
        let _ = start.read_exact(&mut mode);
        drop(start);
        if attached {
            observer::collect(
                pid,
                rules,
                mode[0] == FILTER_INSTALLED,
                launch.is_some(),
                |status, evidence| {
                    // A failed seccomp installation can leave no_new_privs set.
                    // Re-launch from the unfiltered runner before any test code runs.
                    let status = if mode[0] == FILTER_UNAVAILABLE && libc::WIFEXITED(status) {
                        if launch.is_some() {
                            SANDBOX_UNAVAILABLE
                        } else {
                            RETRY_UNFILTERED
                        }
                    } else {
                        status
                    };
                    notify(report, status, evidence);
                },
            );
        } else if let Ok((_, status)) = wait(pid, 0) {
            let status = if launch.is_some() && libc::WIFEXITED(status) {
                SANDBOX_UNAVAILABLE
            } else {
                status
            };
            notify(report, status, json!("unavailable"));
        }
    } else {
        notify(report, LAUNCH_FAILED, json!("unavailable"));
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
