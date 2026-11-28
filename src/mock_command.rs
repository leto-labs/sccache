// Copyright 2016 Mozilla Foundation
//
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

//! Traits and types for mocking process execution.
//!
//! This module provides a set of traits and types that can be used
//! to write code that expects to execute processes using `std::process::Command`
//! in a way that can be mocked for tests.
//!
//! Instead of using `Command::new()`, make your code generic using
//! `CommandCreator` as a trait bound, and use its `new_command` method.
//! `new_command` returns an object implementing `CommandChild`, which
//! mirrors the methods of `Command`.
//!
//! For production use, you can then instantiate your code with
//! `ProcessCommandCreator`, which simply returns `Command::new()` from
//! its `new_command` method.
//!
//! For testing, you can instantiate your code with `MockCommandCreator`,
//! which creates `MockCommand` objects which in turn spawn `MockChild`
//! objects. You can use `MockCommand::next_command_spawns` to provide
//! the result of `spawn` from the next `MockCommand` that it creates.
//! `MockCommandCreator::new_command` will fail an `assert` if it attempts
//! to create a command and does not have any pending `MockChild` objects
//! to hand out, so your tests must provide enough outputs for all
//! expected process executions in the test.
//!
//! If your code under test needs to spawn processes across threads, you
//! can use `CommandCreatorSync` as a trait bound, which is implemented for
//! `ProcessCommandCreator` (since it has no state), and also for
//! `Arc<Mutex<CommandCreator>>`. `CommandCreatorSync` provides a
//! `new_command_sync` method which your code can call to create new
//! objects implementing `CommandChild` in a thread-safe way. Your tests can
//! then create an `Arc<Mutex<MockCommandCreator>>` and safely provide
//! `MockChild` outputs.

use crate::errors::*;
use crate::jobserver::{Client, CommandToken};
use async_trait::async_trait;
use std::boxed::Box;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

/// A trait that provides a subset of the methods of `std::process::Child`.
#[async_trait]
pub trait CommandChild {
    /// The type of the process' standard input.
    type I: AsyncWrite + Unpin + Sync + Send + 'static;
    /// The type of the process' standard output.
    type O: AsyncRead + Unpin + Sync + Send + 'static;
    /// The type of the process' standard error.
    type E: AsyncRead + Unpin + Sync + Send + 'static;

    /// Take the stdin object from the process, if available.
    fn take_stdin(&mut self) -> Option<Self::I>;
    /// Take the stdout object from the process, if available.
    fn take_stdout(&mut self) -> Option<Self::O>;
    /// Take the stderr object from the process, if available.
    fn take_stderr(&mut self) -> Option<Self::E>;
    /// Wait for the process to complete and return its exit status.
    async fn wait(self) -> io::Result<ExitStatus>;
    /// Wait for the process to complete and return its output.
    async fn wait_with_output(self) -> io::Result<Output>;
}

/// A trait that provides a subset of the methods of `std::process::Command`.
#[async_trait]
pub trait RunCommand: fmt::Debug + Send {
    /// The type returned by `spawn`.
    type C: CommandChild + Send + 'static;

    /// Append `arg` to the process commandline.
    fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self;
    /// Append `args` to the process commandline.
    fn args<S: AsRef<OsStr>>(&mut self, args: &[S]) -> &mut Self;
    /// Insert or update an environment variable mapping.
    fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>;
    /// Add or update multiple environment variable mappings.
    fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>;
    /// Clears the entire environment map for the child process.
    fn env_clear(&mut self) -> &mut Self;
    /// Set the working directory of the process to `dir`.
    fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self;
    /// Set the process' stdin from `cfg`.
    fn stdin(&mut self, cfg: Stdio) -> &mut Self;
    /// Set the process' stdout from `cfg`.
    fn stdout(&mut self, cfg: Stdio) -> &mut Self;
    /// Set the process' stderr from `cfg`.
    fn stderr(&mut self, cfg: Stdio) -> &mut Self;
    /// Hand sccache's jobserver down to this child.
    ///
    /// Only worth doing for a child that implements the protocol -- in
    /// practice `rustc`, which reads `CARGO_MAKEFLAGS`. It is not free: see
    /// [`AsyncCommand::spawn`] for why sharing costs a `fork` instead of a
    /// `posix_spawn`.
    fn share_jobserver(&mut self) -> &mut Self {
        self
    }
    /// [`share_jobserver`](RunCommand::share_jobserver), but only if our
    /// jobserver is the build's own `fifo:` one, which costs no `pre_exec`
    /// to hand on.
    fn share_jobserver_if_fifo(&mut self) -> &mut Self {
        self
    }
    /// Execute the process and return a process object.
    async fn spawn(&mut self) -> Result<Self::C>;
}

/// A trait that provides a means to create objects implementing `RunCommand`.
///
/// This is provided so that `MockCommandCreator` can have state for testing.
/// For the non-testing scenario, `ProcessCommandCreator` is simply a unit
/// struct with a trivial implementation of this.
pub trait CommandCreator {
    /// The type returned by `new_command`.
    type Cmd: RunCommand;

    /// Create a new instance of this type.
    fn new(client: &Client) -> Self;
    /// Create a new object that implements `RunCommand` that can be used
    /// to create a new process.
    fn new_command<S: AsRef<OsStr>>(&mut self, program: S) -> Self::Cmd;
}

/// A trait for simplifying the normal case while still allowing the mock case requiring mutability.
pub trait CommandCreatorSync: Clone + Send + Sync + 'static {
    type Cmd: RunCommand;

    fn new(client: &Client) -> Self;

    fn new_command_sync<S: AsRef<OsStr>>(&mut self, program: S) -> Self::Cmd;
}

pub struct Child {
    // Dropped first: the group is killed before Tokio can reap the leader and
    // before the token goes back.
    group: GroupKill,
    #[cfg(target_os = "linux")]
    pidfd: Option<tokio::io::unix::AsyncFd<std::os::unix::io::OwnedFd>>,
    inner: tokio::process::Child,
    _token: CommandToken,
}

/// The daemon's children each lead their own process group. When a compile is
/// cancelled because its client went away, `kill_on_drop` alone would kill
/// only the compiler, and a linker or other helper it started would keep
/// running: holding jobserver tokens taken from the daemon's pipe, and writing
/// its output after a retry has written it.
///
/// The group is always killed before the leader is reaped so its PGID is still
/// pinned by the leader (running or zombie) and cannot be reused by another
/// process:
/// - on drop/cancellation, `GroupKill::drop` runs before `inner` is dropped;
/// - on normal exit on Linux, a `pidfd` notifies us when the leader enters
///   zombie state (verified with `WNOWAIT`), so we kill the group before
///   calling `inner.wait()` to reap it.
struct GroupKill(Option<u32>);

impl GroupKill {
    fn kill(&mut self) {
        let pgid = self.0.take();
        #[cfg(unix)]
        if let Some(pgid) = pgid
            .and_then(|pgid| i32::try_from(pgid).ok())
            .filter(|&pgid| pgid > 1)
        {
            // SAFETY: plain syscall on a group whose leader is not yet reaped.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = pgid;
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(target_os = "linux")]
fn warn_pidfd_open_failed(pid: u32, err: &io::Error) {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    static LAST_WARN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let mut last = LAST_WARN.get_or_init(Default::default).lock().unwrap();
    let now = Instant::now();
    if last.is_none_or(|prev| now.duration_since(prev) >= Duration::from_secs(60)) {
        *last = Some(now);
        drop(last);
        warn!(
            "pidfd_open failed for child {pid}: {err}; normal-exit process-group cleanup \
             disabled for this compile"
        );
    } else {
        drop(last);
        debug!("pidfd_open failed for child {pid}: {err}");
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> io::Result<tokio::io::unix::AsyncFd<std::os::unix::io::OwnedFd>> {
    use std::os::unix::io::{FromRawFd, OwnedFd};
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
    // SAFETY: `pid` is an unreaped child we just spawned; `pidfd_open` sets
    // `O_CLOEXEC` and returns a new owned file descriptor. Flags are `0`
    // (`PIDFD_NONBLOCK` requires Linux 5.10, while `AsyncFd` uses epoll and
    // `waitid` already passes `WNOHANG`).
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pidfd_open` returned a valid, newly allocated descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
    // SAFETY: `owned` is a valid open file descriptor owned exclusively by the
    // returned `AsyncFd` and is never replaced or closed while registered.
    unsafe {
        tokio::io::unix::AsyncFd::register_with_interest(owned, tokio::io::Interest::READABLE)
    }
    .map_err(|err| err.into_parts().1)
}

#[cfg(target_os = "linux")]
fn pidfd_exited(fd: std::os::unix::io::RawFd) -> io::Result<bool> {
    loop {
        // SAFETY: zeroed `siginfo_t` is valid as an out-param for `waitid`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is our live pidfd; `WNOWAIT` inspects exit state
        // without reaping the zombie, keeping the PGID pinned.
        let rc = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                fd as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `si_pid` is populated by `waitid` when a waitable child
            // matched, and zeroed by the kernel when `WNOHANG` found none.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

impl Child {
    async fn wait_and_kill_group(&mut self) -> io::Result<ExitStatus> {
        #[cfg(target_os = "linux")]
        if let Some(pidfd) = self.pidfd.take() {
            use std::os::unix::io::AsRawFd;
            let raw_fd = pidfd.as_raw_fd();
            loop {
                let mut guard = match pidfd.readable().await {
                    Ok(guard) => guard,
                    Err(err) => {
                        debug!("pidfd readability wait failed: {err}");
                        break;
                    }
                };
                match pidfd_exited(raw_fd) {
                    Ok(true) => {
                        self.group.kill();
                        break;
                    }
                    Ok(false) => {
                        guard.clear_ready();
                    }
                    Err(err) => {
                        debug!("waitid(P_PIDFD, WNOWAIT) failed: {err}");
                        break;
                    }
                }
            }
            drop(pidfd);
        }
        let result = self.inner.wait().await;
        // Once the leader is reaped, never send a post-reap kill(-pgid).
        self.group.disarm();
        result
    }
}

/// Trivial implementation of `CommandChild` for `std::process::Child`.
#[async_trait]
impl CommandChild for Child {
    type I = ChildStdin;
    type O = ChildStdout;
    type E = ChildStderr;

    fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin.take()
    }
    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr.take()
    }

    async fn wait(mut self) -> io::Result<ExitStatus> {
        // `self` stays whole, so a cancelled wait drops the fields in order.
        self.wait_and_kill_group().await
    }

    async fn wait_with_output(mut self) -> io::Result<Output> {
        use tokio::io::AsyncReadExt;
        async fn read_all<R: AsyncRead + Unpin>(pipe: Option<R>) -> io::Result<Vec<u8>> {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut buf).await?;
            }
            Ok(buf)
        }
        let stdout = read_all(self.inner.stdout.take());
        let stderr = read_all(self.inner.stderr.take());
        drop(self.inner.stdin.take());
        // Kill the group before the leader is reaped (via pidfd on Linux), so
        // a helper still holding the pipes cannot keep the reads alive.
        let status = self.wait_and_kill_group();
        let (status, stdout, stderr) = futures::future::try_join3(status, stdout, stderr).await?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

pub struct AsyncCommand {
    inner: Option<Command>,
    jobserver: Client,
    share_jobserver: bool,
}

/// Whether the environment set on `cmd` hands it a jobserver opened by path
/// (`--jobserver-auth=fifo:PATH`, as GNU make 4.4+ and Cargo pass on), which,
/// unlike inherited pipe descriptors, any process can reopen.
fn names_fifo_jobserver(cmd: &Command) -> bool {
    // The variables `jobserver::Client::from_env` reads, in its order.
    // `CARGO_MAKEFLAGS` never reaches the server: the client drops it from the
    // environment it sends, so in practice the fifo comes through `MAKEFLAGS`.
    ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"]
        .iter()
        .find_map(|var| {
            cmd.get_envs()
                .find(|(key, _)| key == var)
                .and_then(|(_, value)| value)
        })
        .and_then(OsStr::to_str)
        .is_some_and(|flags| {
            flags
                .split_ascii_whitespace()
                .any(|arg| arg.starts_with("--jobserver-auth=fifo:"))
        })
}

impl AsyncCommand {
    pub fn new<S: AsRef<OsStr>>(program: S, jobserver: Client) -> AsyncCommand {
        AsyncCommand {
            inner: Some(Command::new(program)),
            jobserver,
            share_jobserver: false,
        }
    }

    fn inner(&mut self) -> &mut Command {
        self.inner.as_mut().expect("can't reuse commands")
    }
}

/// Trivial implementation of `RunCommand` for `std::process::Command`.
#[async_trait]
impl RunCommand for AsyncCommand {
    type C = Child;

    fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut AsyncCommand {
        self.inner().arg(arg);
        self
    }
    fn args<S: AsRef<OsStr>>(&mut self, args: &[S]) -> &mut AsyncCommand {
        self.inner().args(args);
        self
    }
    fn env<K, V>(&mut self, key: K, val: V) -> &mut AsyncCommand
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner().env(key, val);
        self
    }
    fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner().envs(vars);
        self
    }
    fn env_clear(&mut self) -> &mut AsyncCommand {
        self.inner().env_clear();
        self
    }
    fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut AsyncCommand {
        self.inner().current_dir(dir);
        self
    }

    fn stdin(&mut self, cfg: Stdio) -> &mut AsyncCommand {
        self.inner().stdin(cfg);
        self
    }
    fn stdout(&mut self, cfg: Stdio) -> &mut AsyncCommand {
        self.inner().stdout(cfg);
        self
    }
    fn stderr(&mut self, cfg: Stdio) -> &mut AsyncCommand {
        self.inner().stderr(cfg);
        self
    }
    fn share_jobserver(&mut self) -> &mut AsyncCommand {
        self.share_jobserver = true;
        self
    }
    fn share_jobserver_if_fifo(&mut self) -> &mut AsyncCommand {
        if self.jobserver.is_fifo() {
            self.share_jobserver = true;
        }
        self
    }
    async fn spawn(&mut self) -> Result<Child> {
        let mut inner = self.inner.take().unwrap();
        // Acquire before configuring so private-pool sharing configures the
        // exact pipe generation that issued our admission token. The token is
        // acquired either way: it rate-limits how many children *we* run, which
        // is separate from whether the child can acquire tokens of its own.
        let token = self.jobserver.acquire_for_command().await?;
        // If the build that asked for this compile named its jobserver by
        // path, the child keeps it and opens it the way it would have without
        // sccache, so it stays within that build's (or a system-wide) limit
        // instead of getting our private pool.
        if !(self.share_jobserver && names_fifo_jobserver(&inner)) {
            inner.env_remove("MAKEFLAGS");
            inner.env_remove("MFLAGS");
            inner.env_remove("CARGO_MAKEFLAGS");
            // `configure` registers a `pre_exec` closure to clear `CLOEXEC` on
            // the jobserver's file descriptors, and any `pre_exec` makes `std`
            // fall back from `posix_spawn` to `fork`+`exec`. Almost nothing
            // sccache spawns can use a jobserver -- preprocessors, version
            // probes and C/C++ compilers all ignore it -- so the default is
            // not to share, and the callers that spawn `rustc` ask for it.
            if self.share_jobserver {
                token.configure(&mut inner);
            }
        }

        // Compiles run by a client stay in its group, so that a Ctrl-C at the
        // terminal still reaches them.
        let own_group = self.jobserver.process_groups;
        #[cfg(unix)]
        if own_group {
            use std::os::unix::process::CommandExt;
            inner.process_group(0);
        }

        let mut inner = tokio::process::Command::from(inner);
        let child = {
            let _spawning = crate::util::spawn_guard();
            inner.kill_on_drop(true).spawn()
        }
        .with_context(|| format!("failed to spawn {:?}", inner))?;

        let pgid = child.id().filter(|_| own_group);
        #[cfg(target_os = "linux")]
        let pidfd = pgid.and_then(|pid| match open_pidfd(pid) {
            Ok(pidfd) => Some(pidfd),
            Err(err) => {
                warn_pidfd_open_failed(pid, &err);
                None
            }
        });

        Ok(Child {
            group: GroupKill(pgid),
            #[cfg(target_os = "linux")]
            pidfd,
            inner: child,
            _token: token,
        })
    }
}

impl fmt::Debug for AsyncCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

/// Struct to use `RunCommand` with `std::process::Command`.
#[derive(Clone)]
pub struct ProcessCommandCreator {
    jobserver: Client,
}

/// Trivial implementation of `CommandCreator` for `ProcessCommandCreator`.
impl CommandCreator for ProcessCommandCreator {
    type Cmd = AsyncCommand;

    fn new(client: &Client) -> ProcessCommandCreator {
        ProcessCommandCreator {
            jobserver: client.clone(),
        }
    }

    fn new_command<S: AsRef<OsStr>>(&mut self, program: S) -> AsyncCommand {
        AsyncCommand::new(program, self.jobserver.clone())
    }
}

/// Trivial implementation of `CommandCreatorSync` for `ProcessCommandCreator`.
impl CommandCreatorSync for ProcessCommandCreator {
    type Cmd = AsyncCommand;

    fn new(client: &Client) -> ProcessCommandCreator {
        CommandCreator::new(client)
    }

    fn new_command_sync<S: AsRef<OsStr>>(&mut self, program: S) -> AsyncCommand {
        // This doesn't actually use any mutable state.
        self.new_command(program)
    }
}

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;

#[cfg(unix)]
pub type ExitStatusValue = i32;
#[cfg(windows)]
pub type ExitStatusValue = u32;

#[allow(dead_code)]
pub fn exit_status(v: ExitStatusValue) -> ExitStatus {
    ExitStatus::from_raw(v)
}

/// A struct that mocks `std::process::Child`.
#[allow(dead_code)]
#[derive(Debug)]
pub struct MockChild {
    //TODO: this doesn't work to actually track writes...
    /// A `Cursor` to hand out as stdin.
    pub stdin: Option<io::Cursor<Vec<u8>>>,
    /// A `Cursor` to hand out as stdout.
    pub stdout: Option<io::Cursor<Vec<u8>>>,
    /// A `Cursor` to hand out as stderr.
    pub stderr: Option<io::Cursor<Vec<u8>>>,
    /// The `Result` to be handed out when `wait` is called.
    pub wait_result: Option<io::Result<ExitStatus>>,
    #[cfg(test)]
    token: Option<CommandToken>,
}

/// A mocked child process that simply returns stored values for its status and output.
impl MockChild {
    /// Create a `MockChild` that will return the specified `status`, `stdout`, and `stderr` when waited upon.
    #[allow(dead_code)]
    pub fn new<T: AsRef<[u8]>, U: AsRef<[u8]>>(
        status: ExitStatus,
        stdout: T,
        stderr: U,
    ) -> MockChild {
        MockChild {
            stdin: Some(io::Cursor::new(vec![])),
            stdout: Some(io::Cursor::new(stdout.as_ref().to_vec())),
            stderr: Some(io::Cursor::new(stderr.as_ref().to_vec())),
            wait_result: Some(Ok(status)),
            #[cfg(test)]
            token: None,
        }
    }

    /// Create a `MockChild` that will return the specified `err` when waited upon.
    #[allow(dead_code)]
    pub fn with_error(err: io::Error) -> MockChild {
        MockChild {
            stdin: None,
            stdout: None,
            stderr: None,
            wait_result: Some(Err(err)),
            #[cfg(test)]
            token: None,
        }
    }
}

#[async_trait]
impl CommandChild for MockChild {
    type I = io::Cursor<Vec<u8>>;
    type O = io::Cursor<Vec<u8>>;
    type E = io::Cursor<Vec<u8>>;

    fn take_stdin(&mut self) -> Option<io::Cursor<Vec<u8>>> {
        self.stdin.take()
    }
    fn take_stdout(&mut self) -> Option<io::Cursor<Vec<u8>>> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<io::Cursor<Vec<u8>>> {
        self.stderr.take()
    }

    async fn wait(mut self) -> io::Result<ExitStatus> {
        self.wait_result.take().unwrap()
    }

    async fn wait_with_output(self) -> io::Result<Output> {
        let MockChild {
            stdout,
            stderr,
            wait_result,
            ..
        } = self;

        wait_result.unwrap().map(|status| Output {
            status,
            stdout: stdout.map(|c| c.into_inner()).unwrap_or_else(Vec::new),
            stderr: stderr.map(|c| c.into_inner()).unwrap_or_else(Vec::new),
        })
    }
}

type MockSpawn = Box<dyn Fn(&[OsString]) -> Result<MockChild> + Send>;

pub enum ChildOrCall {
    Child(Result<MockChild>),
    #[allow(
        dead_code,
        reason = "Read in the async_trait below, but rustc doesn't see it as used"
    )]
    Call(MockSpawn),
    #[cfg(test)]
    WithJobserver(Client, MockSpawn),
}

impl fmt::Debug for ChildOrCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ChildOrCall::Child(ref r) => write!(f, "ChildOrCall::Child({:?}", r),
            ChildOrCall::Call(_) => write!(f, "ChildOrCall::Call(...)"),
            #[cfg(test)]
            ChildOrCall::WithJobserver(..) => write!(f, "ChildOrCall::WithJobserver(...)"),
        }
    }
}

/// A mocked command that simply returns its `child` from `spawn`.
#[allow(dead_code)]
#[derive(Debug)]
pub struct MockCommand {
    pub child: Option<ChildOrCall>,
    pub args: Vec<OsString>,
}

#[async_trait]
impl RunCommand for MockCommand {
    type C = MockChild;

    fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut MockCommand {
        self.args.push(arg.as_ref().to_owned());
        self
    }
    fn args<S: AsRef<OsStr>>(&mut self, args: &[S]) -> &mut MockCommand {
        self.args.extend(args.iter().map(|a| a.as_ref().to_owned()));
        self
    }
    fn env<K, V>(&mut self, _key: K, _val: V) -> &mut MockCommand
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self
    }
    fn envs<I, K, V>(&mut self, _vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self
    }
    fn env_clear(&mut self) -> &mut MockCommand {
        self
    }
    fn current_dir<P: AsRef<Path>>(&mut self, _dir: P) -> &mut MockCommand {
        //TODO: assert value of dir
        self
    }
    fn stdin(&mut self, _cfg: Stdio) -> &mut MockCommand {
        self
    }
    fn stdout(&mut self, _cfg: Stdio) -> &mut MockCommand {
        self
    }
    fn stderr(&mut self, _cfg: Stdio) -> &mut MockCommand {
        self
    }
    async fn spawn(&mut self) -> Result<MockChild> {
        match self.child.take().unwrap() {
            ChildOrCall::Child(c) => c,
            ChildOrCall::Call(f) => f(&self.args),
            #[cfg(test)]
            ChildOrCall::WithJobserver(jobserver, f) => {
                let token = jobserver.acquire_for_command().await?;
                let mut child = f(&self.args)?;
                child.token = Some(token);
                Ok(child)
            }
        }
    }
}

/// `MockCommandCreator` allows mocking out process creation by providing `MockChild` instances to be used in advance.
#[allow(dead_code)]
pub struct MockCommandCreator {
    /// Data to be used as the return value of `MockCommand::spawn`.
    pub children: Vec<ChildOrCall>,
}

impl MockCommandCreator {
    /// The next `MockCommand` created will return `child` from `RunCommand::spawn`.
    #[allow(dead_code)]
    pub fn next_command_spawns(&mut self, child: Result<MockChild>) {
        self.children.push(ChildOrCall::Child(child));
    }

    /// The next `MockCommand` created will call `call` with the command-line
    /// arguments passed to the command.
    #[allow(dead_code)]
    pub fn next_command_calls<C>(&mut self, call: C)
    where
        C: Fn(&[OsString]) -> Result<MockChild> + Send + 'static,
    {
        self.children.push(ChildOrCall::Call(Box::new(call)));
    }
}

impl CommandCreator for MockCommandCreator {
    type Cmd = MockCommand;

    fn new(_client: &Client) -> MockCommandCreator {
        MockCommandCreator {
            children: Vec::new(),
        }
    }

    fn new_command<S: AsRef<OsStr>>(&mut self, _program: S) -> MockCommand {
        assert!(
            !self.children.is_empty(),
            "Too many calls to MockCommandCreator::new_command, or not enough to MockCommandCreator::new_command_spawns!"
        );
        //TODO: assert value of program
        MockCommand {
            child: Some(self.children.remove(0)),
            args: vec![],
        }
    }
}

/// To simplify life for using a `CommandCreator` across multiple threads.
impl<T: CommandCreator + 'static + Send> CommandCreatorSync for Arc<Mutex<T>> {
    type Cmd = T::Cmd;

    fn new(client: &Client) -> Arc<Mutex<T>> {
        Arc::new(Mutex::new(T::new(client)))
    }

    fn new_command_sync<S: AsRef<OsStr>>(&mut self, program: S) -> T::Cmd {
        self.lock().unwrap().new_command(program)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::jobserver::Client;
    use crate::test::utils::*;
    use std::ffi::OsStr;
    use std::io;
    use std::process::{ExitStatus, Output};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn spawn_command<T: CommandCreator, S: AsRef<OsStr>>(
        creator: &mut T,
        program: S,
    ) -> Result<<<T as CommandCreator>::Cmd as RunCommand>::C> {
        creator.new_command(program).spawn().wait()
    }

    fn spawn_wait_command<T: CommandCreator, S: AsRef<OsStr>>(
        creator: &mut T,
        program: S,
    ) -> Result<ExitStatus> {
        Ok(spawn_command(creator, program)?.wait().wait()?)
    }

    fn spawn_output_command<T: CommandCreator, S: AsRef<OsStr>>(
        creator: &mut T,
        program: S,
    ) -> Result<Output> {
        Ok(spawn_command(creator, program)?.wait_with_output().wait()?)
    }

    fn spawn_on_thread<T: CommandCreatorSync + Send + 'static>(
        mut t: T,
        really: bool,
    ) -> ExitStatus {
        thread::spawn(move || {
            if really {
                t.new_command_sync("foo")
                    .spawn()
                    .wait()
                    .unwrap()
                    .wait()
                    .wait()
                    .unwrap()
            } else {
                exit_status(1)
            }
        })
        .join()
        .unwrap()
    }

    #[test]
    fn test_mock_command_wait() {
        let client = Client::new_num(1);
        let mut creator = MockCommandCreator::new(&client);
        creator.next_command_spawns(Ok(MockChild::new(exit_status(0), "hello", "error")));
        assert_eq!(
            0,
            spawn_wait_command(&mut creator, "foo")
                .unwrap()
                .code()
                .unwrap()
        );
    }

    #[test]
    #[should_panic]
    fn test_unexpected_new_command() {
        // If next_command_spawns hasn't been called enough times,
        // new_command should panic.
        let client = Client::new_num(1);
        let mut creator = MockCommandCreator::new(&client);
        creator.new_command("foo").spawn().wait().unwrap();
    }

    #[test]
    fn test_mock_command_output() {
        let client = Client::new_num(1);
        let mut creator = MockCommandCreator::new(&client);
        creator.next_command_spawns(Ok(MockChild::new(exit_status(0), "hello", "error")));
        let output = spawn_output_command(&mut creator, "foo").unwrap();
        assert_eq!(0, output.status.code().unwrap());
        assert_eq!(b"hello".to_vec(), output.stdout);
        assert_eq!(b"error".to_vec(), output.stderr);
    }

    #[test]
    fn test_mock_command_calls() {
        let client = Client::new_num(1);
        let mut creator = MockCommandCreator::new(&client);
        creator.next_command_calls(|_| Ok(MockChild::new(exit_status(0), "hello", "error")));
        let output = spawn_output_command(&mut creator, "foo").unwrap();
        assert_eq!(0, output.status.code().unwrap());
        assert_eq!(b"hello".to_vec(), output.stdout);
        assert_eq!(b"error".to_vec(), output.stderr);
    }

    #[test]
    fn test_mock_spawn_error() {
        let client = Client::new_num(1);
        let mut creator = MockCommandCreator::new(&client);
        creator.next_command_spawns(Err(anyhow!("error")));
        let e = spawn_command(&mut creator, "foo").err().unwrap();
        assert_eq!("error", e.to_string());
    }

    #[test]
    fn test_mock_wait_error() {
        let client = Client::new_num(1);
        let mut creator = MockCommandCreator::new(&client);
        creator.next_command_spawns(Ok(MockChild::with_error(io::Error::other("error"))));
        let e = spawn_wait_command(&mut creator, "foo").err().unwrap();
        assert_eq!("error", e.to_string());
    }

    #[test]
    fn test_mock_command_sync() {
        let client = Client::new_num(1);
        let creator = Arc::new(Mutex::new(MockCommandCreator::new(&client)));
        next_command(
            &creator,
            Ok(MockChild::new(exit_status(0), "hello", "error")),
        );
        assert_eq!(exit_status(0), spawn_on_thread(creator, true));
    }

    /// The jobserver reaches a child through `CARGO_MAKEFLAGS`, so reading it
    /// back out of the child is what "did the child get the jobserver?" means.
    #[cfg(unix)]
    fn child_sees_cargo_makeflags(share: bool) -> bool {
        let client = Client::new_num(1);
        let mut creator = <ProcessCommandCreator as CommandCreator>::new(&client);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut cmd = creator.new_command("/bin/sh");
        cmd.args(&["-c", "printf %s \"$CARGO_MAKEFLAGS\""]);
        if share {
            cmd.share_jobserver();
        }
        let output: std::process::Output = runtime
            .block_on(async { crate::util::run_input_output(cmd, None).await })
            .unwrap();
        !output.stdout.is_empty()
    }

    #[test]
    #[cfg(unix)]
    fn jobserver_is_withheld_by_default() {
        assert!(
            !child_sees_cargo_makeflags(false),
            "a child should not inherit the jobserver unless it asks"
        );
    }

    #[test]
    #[cfg(unix)]
    fn share_jobserver_hands_it_over() {
        assert!(
            child_sees_cargo_makeflags(true),
            "share_jobserver() should give the child CARGO_MAKEFLAGS"
        );
    }

    /// `(CARGO_MAKEFLAGS, MAKEFLAGS)` as the child sees them, when the
    /// compile's own environment (as the server sets it from the client's)
    /// has `var` set to `flags`.
    #[cfg(unix)]
    fn child_makeflags_given(var: &str, flags: &str, share: bool) -> (String, String) {
        let client = Client::new_num(1);
        let mut creator = <ProcessCommandCreator as CommandCreator>::new(&client);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut cmd = creator.new_command("/bin/sh");
        cmd.args(&["-c", "printf '%s\\n%s' \"$CARGO_MAKEFLAGS\" \"$MAKEFLAGS\""])
            .env_clear()
            .env(var, flags);
        if share {
            cmd.share_jobserver();
        }
        let output: std::process::Output = runtime
            .block_on(async { crate::util::run_input_output(cmd, None).await })
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let (cargo_makeflags, makeflags) = stdout.split_once('\n').unwrap();
        (cargo_makeflags.to_owned(), makeflags.to_owned())
    }

    #[test]
    #[cfg(unix)]
    fn share_jobserver_keeps_a_fifo_jobserver() {
        let flags = "-j8 --jobserver-auth=fifo:/tmp/GMfifo1234";
        // The client strips `CARGO_MAKEFLAGS`, so the fifo normally arrives
        // through `MAKEFLAGS`; both are honoured.
        assert_eq!(
            child_makeflags_given("MAKEFLAGS", flags, true),
            (String::new(), flags.to_owned())
        );
        assert_eq!(
            child_makeflags_given("CARGO_MAKEFLAGS", flags, true),
            (flags.to_owned(), String::new())
        );
        for var in ["MAKEFLAGS", "CARGO_MAKEFLAGS"] {
            assert_eq!(
                child_makeflags_given(var, flags, false),
                (String::new(), String::new()),
                "a child that doesn't ask for a jobserver still gets none"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn share_jobserver_replaces_an_inherited_pipe_jobserver() {
        let flags = "-j8 --jobserver-fds=1000,1001 --jobserver-auth=1000,1001";
        for var in ["MAKEFLAGS", "CARGO_MAKEFLAGS"] {
            let (cargo_makeflags, makeflags) = child_makeflags_given(var, flags, true);
            assert_eq!(makeflags, "", "the inherited MAKEFLAGS is dropped");
            let auth = cargo_makeflags
                .split_ascii_whitespace()
                .find_map(|arg| arg.strip_prefix("--jobserver-auth="))
                .unwrap_or_else(|| panic!("no --jobserver-auth in {cargo_makeflags:?}"));
            assert_ne!(auth, "1000,1001", "the child gets our jobserver instead");
        }
    }

    #[test]
    fn fifo_jobserver_detection() {
        let names = |vars: &[(&str, &str)]| {
            let mut cmd = Command::new("true");
            cmd.env_clear();
            for (key, value) in vars {
                cmd.env(key, value);
            }
            names_fifo_jobserver(&cmd)
        };
        assert!(names(&[(
            "CARGO_MAKEFLAGS",
            "--jobserver-auth=fifo:/dev/x"
        )]));
        assert!(names(&[("MAKEFLAGS", " -j8 --jobserver-auth=fifo:/dev/x")]));
        assert!(!names(&[("CARGO_MAKEFLAGS", "--jobserver-auth=3,4")]));
        assert!(!names(&[]));
        // `CARGO_MAKEFLAGS` wins, as in `jobserver::Client::from_env`.
        assert!(!names(&[
            ("CARGO_MAKEFLAGS", "--jobserver-auth=3,4"),
            ("MAKEFLAGS", "--jobserver-auth=fifo:/dev/x"),
        ]));
    }

    /// The daemon's cancelled compile: its client went away, so the child is
    /// dropped. A helper it started (a linker, say) must die with it, not
    /// write its output later, whether the child was never waited for or its
    /// wait was cancelled. A helper that outlives its leader dies when the
    /// leader is reaped, even while it holds the output pipes. A client's own
    /// children stay in its process group.
    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_child_leaves_nothing_behind() {
        use std::time::Duration;
        use tokio::io::AsyncBufReadExt;

        let dir = tempfile::tempdir().unwrap();
        let marker = |name: &str| dir.path().join(name);
        // The helper writes the marker after a second unless it is killed.
        let command = |client: &Client, name: &str, leader: &str| {
            let mut cmd =
                <ProcessCommandCreator as CommandCreatorSync>::new(client).new_command_sync("bash");
            cmd.arg("-c")
                .arg(format!(
                    "(sleep 1; touch {}; sleep 30) & echo started; {leader}",
                    marker(name).display()
                ))
                .stdout(Stdio::piped());
            cmd
        };
        async fn started(child: &mut Child) {
            let mut line = String::new();
            let mut stdout = tokio::io::BufReader::new(child.take_stdout().unwrap());
            stdout.read_line(&mut line).await.unwrap();
            assert_eq!(line, "started\n");
            child.inner.stdout = Some(stdout.into_inner());
        }
        let settle = || tokio::time::sleep(Duration::from_secs(2));

        let mut daemon = Client::new_num(2).with_compile_queue();
        daemon.process_groups = true;
        let mut child = command(&daemon, "dropped", "exec sleep 30")
            .spawn()
            .await
            .unwrap();
        let pid = child.inner.id().unwrap() as i32;
        // SAFETY: plain syscalls on our own children.
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        started(&mut child).await;
        drop(child);

        let mut child = command(&daemon, "cancelled", "exec sleep 30")
            .spawn()
            .await
            .unwrap();
        started(&mut child).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), child.wait())
                .await
                .is_err()
        );

        // The leader exits at once; its helper still holds stdout. On Linux a
        // pidfd notifies us before reaping so the group is killed while the
        // zombie still pins the PGID.
        #[cfg(target_os = "linux")]
        {
            let mut child = command(&daemon, "outlived", "exit 0")
                .spawn()
                .await
                .unwrap();
            assert!(child.pidfd.is_some());
            started(&mut child).await;
            let begun = std::time::Instant::now();
            let output = child.wait_with_output().await.unwrap();
            assert!(output.status.success());
            assert!(
                begun.elapsed() < Duration::from_secs(5),
                "{:?}",
                begun.elapsed()
            );

            // Same pre-reap kill on plain `wait()`.
            let mut child = command(&daemon, "outlived-wait", "exit 0")
                .spawn()
                .await
                .unwrap();
            started(&mut child).await;
            assert!(child.wait().await.unwrap().success());

            // Fallback when `pidfd` is unavailable: drop/cancel still kills the
            // group before reaping, while normal `wait()` disarms without a
            // post-reap kill(-pgid).
            let mut child = command(&daemon, "no-pidfd-drop", "exec sleep 30")
                .spawn()
                .await
                .unwrap();
            child.pidfd = None;
            started(&mut child).await;
            drop(child);

            let mut child = command(&daemon, "no-pidfd-wait", "exit 0")
                .spawn()
                .await
                .unwrap();
            child.pidfd = None;
            started(&mut child).await;
            assert!(child.wait().await.unwrap().success());
        }

        // Normal exit with no background helper is unaffected.
        let mut normal =
            <ProcessCommandCreator as CommandCreatorSync>::new(&daemon).new_command_sync("bash");
        normal.args(&["-c", "printf ok"]).stdout(Stdio::piped());
        let output = normal
            .spawn()
            .await
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");

        settle().await;
        for name in ["dropped", "cancelled"] {
            assert!(!marker(name).exists(), "{name}");
        }
        #[cfg(target_os = "linux")]
        {
            for name in ["outlived", "outlived-wait", "no-pidfd-drop"] {
                assert!(!marker(name).exists(), "{name}");
            }
            assert!(marker("no-pidfd-wait").exists());
        }

        let client = Client::new_num(2);
        let mut child = command(&client, "client", "exec sleep 30")
            .spawn()
            .await
            .unwrap();
        let pid = child.inner.id().unwrap() as i32;
        assert_eq!(unsafe { libc::getpgid(pid) }, unsafe { libc::getpgid(0) });
        started(&mut child).await;
        drop(child);
        settle().await;
        // Outside the daemon only the leader is killed, as before.
        assert!(marker("client").exists());
    }

    /// After a pool swap, a child spawned with `share_jobserver` receives the
    /// new pipe's fds and can acquire and return tokens on it. Also verifies
    /// that `spawn_guard` is not held while waiting for a jobserver token.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn child_after_swap_uses_new_pipe_and_token_wait_does_not_hold_spawn_guard() {
        use std::time::Duration;

        let client = Client::new_num(2).with_compile_queue();
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let old_pipe = pool.current_pipe_for_test();

        // Hold both tokens on gen 0 so a spawn blocks waiting for a token.
        let held1 = client.acquire_for_command().await.unwrap();
        let held2 = client.acquire_for_command().await.unwrap();
        let mut waiting_cmd =
            <ProcessCommandCreator as CommandCreatorSync>::new(&client).new_command_sync("bash");
        waiting_cmd
            .arg("-c")
            .arg(
                r#"auth=${CARGO_MAKEFLAGS##*--jobserver-auth=}; auth=${auth%% *}
                   IFS= read -r -n1 -u "${auth%,*}" token || exit 1
                   printf %s "$token" >&"${auth#*,}"
                   printf ok"#,
            )
            .stdout(Stdio::piped())
            .share_jobserver();

        let waiter = tokio::spawn(async move {
            let child = waiting_cmd.spawn().await.unwrap();
            child.wait_with_output().await.unwrap()
        });

        // Wait until the spawn task is waiting for a token.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while queue.snapshot().token_wait.active == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(queue.snapshot().token_wait.active, 1);

        // `spawn_guard` must not be held while waiting for a token, so
        // `wait_for_inflight_spawns` returns immediately.
        let guard_check = tokio::task::spawn_blocking(crate::util::wait_for_inflight_spawns);
        tokio::time::timeout(Duration::from_millis(500), guard_check)
            .await
            .expect("wait_for_inflight_spawns blocked on a token waiter")
            .unwrap();

        // Swap the pool while old tokens are still held on `old_pipe`: the
        // waiting spawn must be admitted on the new pipe and its child must
        // read and return a token from the new pipe's advertised fds.
        pool.force_swap_for_test().unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("post-swap child timed out (configured with stale pipe?)")
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"ok");
        assert_eq!(old_pipe.available().unwrap(), 0);
        assert_eq!(pool.current_pipe_for_test().available().unwrap(), 2);

        drop((held1, held2));
        assert_eq!(old_pipe.available().unwrap(), 2);

        // Forwarded `fifo:` jobservers still take a daemon admission token
        // while keeping the caller's `--jobserver-auth=fifo:...` intact.
        let held1 = client.acquire_for_command().await.unwrap();
        let held2 = client.acquire_for_command().await.unwrap();
        let fifo_flags = "-j8 --jobserver-auth=fifo:/tmp/GMfifo-test";
        let mut fifo_cmd =
            <ProcessCommandCreator as CommandCreatorSync>::new(&client).new_command_sync("bash");
        fifo_cmd
            .args(&["-c", "printf '%s|%s' \"$MAKEFLAGS\" \"$CARGO_MAKEFLAGS\""])
            .env_clear()
            .env("MAKEFLAGS", fifo_flags)
            .stdout(Stdio::piped())
            .share_jobserver();
        let fifo_waiter = tokio::spawn(async move {
            let child = fifo_cmd.spawn().await.unwrap();
            child.wait_with_output().await.unwrap()
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while queue.snapshot().token_wait.active == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(queue.snapshot().token_wait.active, 1);
        assert!(!fifo_waiter.is_finished());
        drop(held1);
        let output = tokio::time::timeout(Duration::from_secs(5), fifo_waiter)
            .await
            .expect("forwarded fifo child timed out after token release")
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{fifo_flags}|")
        );
        drop(held2);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_fifo_uses_semaphore_admission_and_forwards_fifo_path() {
        use std::time::Duration;
        let host_path = std::path::PathBuf::from("/run/user/1000/nonexistent-host-jobserver");
        let client = Client::new_host_fifo(1, host_path.clone()).with_compile_queue();
        let queue = client.compile_queue().unwrap();

        // Admission succeeds even when the host FIFO path does not exist (never
        // opens or acquires from the host FIFO, avoiding cargo<->sccache deadlock).
        let held = client.acquire_for_command().await.unwrap();
        let snap = queue.snapshot();
        assert_eq!(snap.token_capacity, 1);
        assert_eq!(snap.running, 1);
        assert_eq!(snap.tokens_in_use, Some(1));
        assert_eq!(snap.pool_swaps, None);

        // Second spawn waits on the in-process semaphore, then receives
        // `--jobserver-auth=fifo:<host_path>` in CARGO_MAKEFLAGS when released.
        let mut cmd =
            <ProcessCommandCreator as CommandCreatorSync>::new(&client).new_command_sync("bash");
        cmd.args(&["-c", "printf '%s|%s' \"$MAKEFLAGS\" \"$CARGO_MAKEFLAGS\""])
            .env_clear()
            .env("CARGO_MAKEFLAGS", "-j8 --jobserver-auth=99,100")
            .stdout(Stdio::piped())
            .share_jobserver();
        let waiter = tokio::spawn(async move {
            let child = cmd.spawn().await.unwrap();
            child.wait_with_output().await.unwrap()
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while queue.snapshot().token_wait.active == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(queue.snapshot().token_wait.active, 1);
        drop(held);

        let output = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("host_fifo child timed out after semaphore release")
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("|-j --jobserver-auth=fifo:{}", host_path.display())
        );
        assert_eq!(queue.snapshot().tokens_in_use, Some(0));

        // Without share_jobserver(), flags are still stripped.
        let mut no_share =
            <ProcessCommandCreator as CommandCreatorSync>::new(&client).new_command_sync("bash");
        no_share
            .args(&["-c", "printf '%s|%s' \"$MAKEFLAGS\" \"$CARGO_MAKEFLAGS\""])
            .env_clear()
            .env("CARGO_MAKEFLAGS", "-j8 --jobserver-auth=99,100")
            .stdout(Stdio::piped());
        let output = no_share
            .spawn()
            .await
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"|");
    }
}
