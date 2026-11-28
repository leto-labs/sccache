use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant as StdInstant};

use tokio::sync::{oneshot, watch};

use crate::compile_queue::{CompileQueue, TokenUse};
use crate::config::JobserverSwap;
use crate::errors::*;

// The execution model of sccache is that on the first run it spawns a server
// in the background and detaches it.
// When normally executing the rust compiler from either cargo or make, it
// will use cargo/make's jobserver and limit its resource usage accordingly.
// When executing the rust compiler through the sccache server, that jobserver
// is not available, and spawning as many rustc as there are CPUs can lead to
// a quadratic use of the CPU resources (each rustc spawning as many threads
// as there are CPUs).
// One way around this issue is to inherit the jobserver from cargo or make
// when the sccache server is spawned, but that means that in some cases, the
// cargo or make process can't terminate until the sccache server terminates
// after its idle timeout (which also never happens if SCCACHE_IDLE_TIMEOUT=0).
// Also, if the sccache server ends up shared between multiple runs of
// cargo/make, then which jobserver is used doesn't make sense anymore.
// Ideally, the sccache client would give a handle to the jobserver it has
// access to, so that the rust compiler would "just" use the jobserver it
// would have used if it had run without sccache, but that adds some extra
// complexity, and requires to use Unix domain sockets.
// What we do instead is to arbitrary use our own jobserver.
// Unfortunately, that doesn't absolve us from having to deal with the original
// jobserver, because make may give us file descriptors to its pipes, and the
// simple fact of keeping them open can block it. That is handled by closing
// every inherited descriptor when the server detaches; see
// `util::close_inherited_fds`.

/// Maximum number of unconfirmed or in-flight helper retirements allowed before
/// further pool swaps are disabled. Keep in sync with `leto/health/health-report.py`.
pub(crate) const MAX_RETIRE_PENDING: u64 = 4;

type TokenSender = oneshot::Sender<io::Result<jobserver::Acquired>>;

const HELPER_ACTIVE: u8 = 0;
const HELPER_RETIRING: u8 = 1;
const HELPER_EXITED: u8 = 2;

/// A single jobserver pipe (or inherited FIFO). Tokens and `Command::pre_exec`
/// configuration hold an `Arc<Pipe>` so the underlying descriptors stay open
/// until the last holder drops.
pub(crate) struct Pipe {
    inner: jobserver::Client,
}

impl Pipe {
    fn new(capacity: usize) -> io::Result<Self> {
        Ok(Self {
            inner: jobserver::Client::new(capacity)?,
        })
    }

    fn from_client(inner: jobserver::Client) -> Self {
        Self { inner }
    }

    pub(crate) fn configure(&self, cmd: &mut Command) {
        self.inner.configure(cmd);
    }

    pub(crate) fn available(&self) -> io::Result<usize> {
        self.inner.available()
    }
}

struct HelperExit {
    state: AtomicU8,
    retire_pending: Arc<AtomicU64>,
}

impl HelperExit {
    fn new(retire_pending: Arc<AtomicU64>) -> Self {
        Self {
            state: AtomicU8::new(HELPER_ACTIVE),
            retire_pending,
        }
    }

    fn mark_retiring(&self) {
        if self
            .state
            .compare_exchange(
                HELPER_ACTIVE,
                HELPER_RETIRING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.retire_pending.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn is_exited(&self) -> bool {
        self.state.load(Ordering::Acquire) == HELPER_EXITED
    }
}

struct HelperExitGuard(Arc<HelperExit>);

impl Drop for HelperExitGuard {
    fn drop(&mut self) {
        if self.0.state.swap(HELPER_EXITED, Ordering::AcqRel) == HELPER_RETIRING {
            self.0.retire_pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

struct Helper {
    // Dropped before `thread` so the request channel closes before the helper
    // thread is interrupted and joined.
    tx: Option<std_mpsc::Sender<TokenSender>>,
    thread: jobserver::HelperThread,
    exit: Arc<HelperExit>,
}

impl Helper {
    fn new(
        pipe: &Pipe,
        retire_pending: Arc<AtomicU64>,
        #[cfg(test)] callback_blocker: Option<std_mpsc::Receiver<()>>,
    ) -> io::Result<Self> {
        let (tx, rx) = std_mpsc::channel::<TokenSender>();
        let exit = Arc::new(HelperExit::new(retire_pending));
        let guard = HelperExitGuard(exit.clone());
        #[cfg(test)]
        let mut blocker = callback_blocker;
        let thread = pipe.inner.clone().into_helper_thread(move |token| {
            // Keep `guard` owned by this closure so it drops only when the
            // helper thread itself exits.
            let _guard = &guard;
            #[cfg(test)]
            if let Some(gate) = blocker.take() {
                let _ = gate.recv();
            }
            if let Ok(sender) = rx.recv() {
                // Dropping a failed send returns the token to its own pipe.
                drop(sender.send(token));
            }
        })?;
        Ok(Self {
            tx: Some(tx),
            thread,
            exit,
        })
    }

    fn request_token(&self, sender: TokenSender) {
        if let Some(tx) = &self.tx
            && tx.send(sender).is_ok()
        {
            self.thread.request_token();
        }
    }

    fn mark_retiring(&self) {
        self.exit.mark_retiring();
    }

    fn close_requests(&mut self) {
        self.tx = None;
    }

    fn retire_blocking(self) {
        let exit = self.exit;
        drop(self.tx);
        drop(self.thread);
        if !exit.is_exited() {
            let pending = exit.retire_pending.load(Ordering::Acquire);
            error!("jobserver helper thread did not exit during retirement ({pending} pending)");
        }
    }
}

/// Ensures a retired `Helper` is never dropped inline on a Tokio worker if
/// spawning a fallback retirement thread fails (`Builder::spawn` drops its
/// closure on error).
struct NoInlineDrop(Option<Helper>);

impl Drop for NoInlineDrop {
    fn drop(&mut self) {
        if let Some(helper) = self.0.take() {
            // Leave `retire_pending` incremented rather than blocking a Tokio
            // worker for up to ~1 s in `HelperThread::drop`.
            std::mem::forget(helper);
        }
    }
}

/// Dedicated OS thread that drops retired helpers so `HelperThread::drop`
/// (which may block up to ~1 s trying to join a stuck helper) never runs on a
/// Tokio worker thread.
#[derive(Default)]
struct Retirer {
    tx: Mutex<Option<std_mpsc::Sender<Helper>>>,
}

impl Retirer {
    fn ensure_started(&self) -> io::Result<()> {
        let mut guard = self.tx.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let (tx, rx) = std_mpsc::channel::<Helper>();
        std::thread::Builder::new()
            .name("jobserver-retire".to_owned())
            .spawn(move || {
                while let Ok(helper) = rx.recv() {
                    helper.retire_blocking();
                }
            })?;
        *guard = Some(tx);
        Ok(())
    }

    fn retire(&self, mut helper: Helper) {
        helper.close_requests();
        let tx = self.tx.lock().unwrap().clone();
        let helper = match tx {
            Some(tx) => match tx.send(helper) {
                Ok(()) => return,
                Err(std_mpsc::SendError(helper)) => helper,
            },
            None => helper,
        };
        let mut wrapped = NoInlineDrop(Some(helper));
        if let Err(err) = std::thread::Builder::new()
            .name("jobserver-retire-fallback".to_owned())
            .spawn(move || {
                if let Some(helper) = wrapped.0.take() {
                    helper.retire_blocking();
                }
            })
        {
            error!("failed to spawn fallback jobserver retirement thread: {err}");
        }
    }
}

struct Generation {
    id: u64,
    created: StdInstant,
    pipe: Arc<Pipe>,
    helper: Helper,
}

pub(crate) struct Candidate {
    pipe: Arc<Pipe>,
    helper: Helper,
}

pub(crate) struct SwapPublished {
    pub(crate) new_id: u64,
    pub(crate) old_in_use: Option<usize>,
    pub(crate) old_age: Duration,
}

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    after_subscribe: Option<std::collections::VecDeque<Box<dyn FnOnce() + Send>>>,
    after_snapshot: Option<std::collections::VecDeque<Box<dyn FnOnce() + Send>>>,
    before_commit: Option<std::collections::VecDeque<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    fail_candidate: bool,
    candidate_entered: Option<oneshot::Sender<()>>,
    candidate_gate: Option<std_mpsc::Receiver<()>>,
    next_helper_blocker: Option<std_mpsc::Receiver<()>>,
}

pub(crate) struct Pool {
    capacity: usize,
    current: Mutex<Generation>,
    id_tx: watch::Sender<u64>,
    pool_swaps: AtomicU64,
    retire_pending: Arc<AtomicU64>,
    retirer: Retirer,
    #[cfg(test)]
    hooks: Mutex<TestHooks>,
    #[cfg(test)]
    acquire_passes: AtomicU64,
}

impl Pool {
    fn new(capacity: usize, pipe: Arc<Pipe>) -> io::Result<Self> {
        let retire_pending = Arc::new(AtomicU64::new(0));
        let helper = Helper::new(
            &pipe,
            retire_pending.clone(),
            #[cfg(test)]
            None,
        )?;
        let (id_tx, _) = watch::channel(0_u64);
        Ok(Self {
            capacity,
            current: Mutex::new(Generation {
                id: 0,
                created: StdInstant::now(),
                pipe,
                helper,
            }),
            id_tx,
            pool_swaps: AtomicU64::new(0),
            retire_pending,
            retirer: Retirer::default(),
            #[cfg(test)]
            hooks: Mutex::default(),
            #[cfg(test)]
            acquire_passes: AtomicU64::new(0),
        })
    }

    pub(crate) fn supports_swap() -> bool {
        // On macOS `jobserver` creates pipes with `pipe()` + `fcntl(FD_CLOEXEC)`,
        // which can race with a concurrent fork; limit swapping to Linux where
        // `pipe2(O_CLOEXEC)` is atomic.
        cfg!(target_os = "linux")
    }

    /// Runs `f` while holding `current`'s mutex. Callers that also lock
    /// `CompileQueue::state` must acquire it inside `f` to preserve the
    /// `pool.current -> queue.state` lock order.
    pub(crate) fn with_current<R>(&self, f: impl FnOnce(u64, Option<usize>) -> R) -> R {
        let guard = self.current.lock().unwrap();
        let available = if cfg!(unix) {
            guard.pipe.available().ok()
        } else {
            None
        };
        f(guard.id, available)
    }

    pub(crate) fn pool_swaps(&self) -> Option<u64> {
        Self::supports_swap().then(|| self.pool_swaps.load(Ordering::Acquire))
    }

    pub(crate) fn retire_pending(&self) -> Option<u64> {
        Self::supports_swap().then(|| self.retire_pending.load(Ordering::Acquire))
    }

    pub(crate) fn retire_pending_count(&self) -> u64 {
        self.retire_pending.load(Ordering::Acquire)
    }

    #[cfg(all(test, unix))]
    fn current_pipe(&self) -> Arc<Pipe> {
        self.current.lock().unwrap().pipe.clone()
    }

    /// Builds a replacement generation outside all pool and queue locks.
    /// Must be called on a blocking thread (`spawn_blocking`).
    pub(crate) fn build_candidate(&self) -> io::Result<Candidate> {
        #[cfg(test)]
        {
            let (fail, entered, gate) = {
                let mut hooks = self.hooks.lock().unwrap();
                (
                    hooks.fail_candidate,
                    hooks.candidate_entered.take(),
                    hooks.candidate_gate.take(),
                )
            };
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            if fail {
                return Err(io::Error::other("injected candidate creation failure"));
            }
        }
        self.retirer.ensure_started()?;
        let pipe = Arc::new(Pipe::new(self.capacity)?);
        #[cfg(test)]
        let blocker = self.hooks.lock().unwrap().next_helper_blocker.take();
        let helper = Helper::new(
            &pipe,
            self.retire_pending.clone(),
            #[cfg(test)]
            blocker,
        )?;
        Ok(Candidate { pipe, helper })
    }

    /// Re-verifies the generation id, retirement cap, and trigger predicate
    /// under `current`'s mutex, then publishes `candidate` and retires the old
    /// helper on the dedicated retirement thread.
    pub(crate) fn try_publish_swap(
        &self,
        expected_id: u64,
        candidate: Candidate,
        verify: impl FnOnce(Option<usize>) -> bool,
    ) -> std::result::Result<SwapPublished, Candidate> {
        let mut guard = self.current.lock().unwrap();
        if guard.id != expected_id
            || self.retire_pending.load(Ordering::Acquire) >= MAX_RETIRE_PENDING
        {
            return Err(candidate);
        }
        let available = if cfg!(unix) {
            guard.pipe.available().ok()
        } else {
            None
        };
        if !verify(available) {
            return Err(candidate);
        }
        let new_id = guard.id + 1;
        let old_in_use = available.map(|avail| self.capacity.saturating_sub(avail));
        let old_age = guard.created.elapsed();
        let old_gen = std::mem::replace(
            &mut *guard,
            Generation {
                id: new_id,
                created: StdInstant::now(),
                pipe: candidate.pipe,
                helper: candidate.helper,
            },
        );
        self.pool_swaps.fetch_add(1, Ordering::AcqRel);
        old_gen.helper.mark_retiring();
        // `send_replace` stores `new_id` even when no waiters are subscribed
        // (such as during an idle-deficit swap); `send` would drop it.
        self.id_tx.send_replace(new_id);
        drop(guard);

        self.retirer.retire(old_gen.helper);
        drop(old_gen.pipe);
        Ok(SwapPublished {
            new_id,
            old_in_use,
            old_age,
        })
    }

    #[cfg(test)]
    fn pop_after_subscribe(&self) -> Option<Box<dyn FnOnce() + Send>> {
        self.hooks
            .lock()
            .unwrap()
            .after_subscribe
            .as_mut()
            .and_then(|q| q.pop_front())
    }

    #[cfg(test)]
    fn pop_after_snapshot(&self) -> Option<Box<dyn FnOnce() + Send>> {
        self.hooks
            .lock()
            .unwrap()
            .after_snapshot
            .as_mut()
            .and_then(|q| q.pop_front())
    }

    #[cfg(test)]
    fn pop_before_commit(&self) -> Option<(oneshot::Sender<()>, oneshot::Receiver<()>)> {
        self.hooks
            .lock()
            .unwrap()
            .before_commit
            .as_mut()
            .and_then(|q| q.pop_front())
    }

    async fn acquire_inner(&self, queue: Option<&Arc<CompileQueue>>) -> Result<CommandToken> {
        let mut usage = queue.map(|queue| queue.start_token());
        // Only touch the Tokio time driver when tracking a compile queue.
        let mut warn_sleep = usage
            .as_ref()
            .map(|usage| Box::pin(tokio::time::sleep_until(usage.warning_deadline())));

        loop {
            #[cfg(test)]
            self.acquire_passes.fetch_add(1, Ordering::Relaxed);
            // Subscribe to the generation id before snapshotting so no swap can
            // slip between the snapshot and waiting for a change.
            let mut id_rx = self.id_tx.subscribe();
            #[cfg(test)]
            if let Some(hook) = self.pop_after_subscribe() {
                hook();
            }

            let (snap_id, pipe, mut myrx) = {
                let guard = self.current.lock().unwrap();
                let (mytx, myrx) = oneshot::channel();
                guard.helper.request_token(mytx);
                (guard.id, guard.pipe.clone(), myrx)
            };

            #[cfg(test)]
            if let Some(hook) = self.pop_after_snapshot() {
                hook();
            }

            loop {
                tokio::select! {
                    res = &mut myrx => {
                        match res {
                            Ok(Ok(acquired)) => {
                                #[cfg(test)]
                                if let Some((entered, gate)) = self.pop_before_commit() {
                                    let _ = entered.send(());
                                    let _ = gate.await;
                                }
                                let guard = self.current.lock().unwrap();
                                if guard.id == snap_id {
                                    if let Some(usage) = usage.as_mut() {
                                        usage.acquired();
                                    }
                                    drop(guard);
                                    return Ok(CommandToken {
                                        _usage: usage,
                                        token: Acquired {
                                            _token: Some(acquired),
                                            pipe: Some(pipe),
                                        },
                                    });
                                }
                                // Stale generation: drop token back to its old pipe and retry.
                                drop(guard);
                                drop(acquired);
                                break;
                            }
                            Ok(Err(err)) => {
                                return Err(err).context("failed to acquire jobserver token");
                            }
                            Err(_) => {
                                if self.current.lock().unwrap().id != snap_id {
                                    break;
                                }
                                anyhow::bail!("jobserver helper panicked");
                            }
                        }
                    }
                    changed = async {
                        id_rx.wait_for(|&id| id > snap_id).await.map(|_| ())
                    } => {
                        drop(myrx);
                        if changed.is_err() {
                            anyhow::bail!("jobserver pool closed");
                        }
                        break;
                    }
                    () = async {
                        if let Some(sleep) = warn_sleep.as_mut() {
                            sleep.await;
                        }
                    }, if warn_sleep.is_some() => {
                        warn_sleep = None;
                        if let Some(usage) = usage.as_ref() {
                            usage.warn_if_slow();
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
enum ClientKind {
    Fifo(Arc<Pipe>),
    Pool(Arc<Pool>),
}

#[derive(Clone)]
pub struct Client {
    kind: ClientKind,
    capacity: usize,
    compile_queue: Option<Arc<CompileQueue>>,
    /// Whether children are started in process groups of their own, to be
    /// killed whole (see `mock_command::GroupKill`).
    pub(crate) process_groups: bool,
}

pub struct Acquired {
    _token: Option<jobserver::Acquired>,
    pipe: Option<Arc<Pipe>>,
}

pub(crate) struct CommandToken {
    _usage: Option<TokenUse>,
    token: Acquired,
}

impl CommandToken {
    pub(crate) fn configure(&self, cmd: &mut Command) {
        if let Some(pipe) = &self.token.pipe {
            pipe.configure(cmd);
        }
    }
}

#[cfg(test)]
impl std::fmt::Debug for CommandToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandToken").finish_non_exhaustive()
    }
}

impl Client {
    pub fn new() -> Client {
        Client::new_num(crate::util::num_cpus())
    }

    /// The jobserver of the build that ran us, if it is one opened by path
    /// (`--jobserver-auth=fifo:PATH`, GNU make 4.4 and later). The problems
    /// above are about inherited pipe descriptors; a path holds nothing of
    /// the build's open, so the client, which lives only as long as one
    /// compile, can use it and hand it on to what it runs. Whoever ran us
    /// already holds a token for this compile, so none is taken per spawn.
    pub fn from_env_fifo() -> Option<Client> {
        if !env_names_fifo() {
            return None;
        }
        // SAFETY: a `fifo:` jobserver is opened by path; no inherited file
        // descriptors are taken over.
        let inner = unsafe { jobserver::Client::from_env() }?;
        Some(Client {
            kind: ClientKind::Fifo(Arc::new(Pipe::from_client(inner))),
            capacity: 0,
            compile_queue: None,
            process_groups: false,
        })
    }

    pub fn new_num(num: usize) -> Client {
        let pipe = Arc::new(Pipe::new(num).expect("failed to create jobserver"));
        let pool = Arc::new(Pool::new(num, pipe).expect("failed to spawn helper thread"));
        Client {
            kind: ClientKind::Pool(pool),
            capacity: num,
            compile_queue: None,
            process_groups: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_compile_queue(self) -> Self {
        self.with_compile_queue_swap(JobserverSwap::default())
    }

    pub(crate) fn with_compile_queue_swap(mut self, swap_policy: JobserverSwap) -> Self {
        let pool = match &self.kind {
            ClientKind::Pool(pool) => Some(pool.clone()),
            ClientKind::Fifo(_) => None,
        };
        self.compile_queue = Some(Arc::new(CompileQueue::new(
            self.capacity,
            pool,
            swap_policy,
        )));
        // Only the daemon runs compiles for clients that can go away. Run in
        // the foreground at a terminal, it keeps one group: a child in a
        // background group that read the terminal would stop.
        self.process_groups = cfg!(unix) && !std::io::IsTerminal::is_terminal(&std::io::stdin());
        self
    }

    pub(crate) fn compile_queue(&self) -> Option<Arc<CompileQueue>> {
        self.compile_queue.clone()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn pool(&self) -> Option<Arc<Pool>> {
        match &self.kind {
            ClientKind::Pool(pool) => Some(pool.clone()),
            ClientKind::Fifo(_) => None,
        }
    }

    pub(crate) async fn acquire_for_command(&self) -> Result<CommandToken> {
        match &self.kind {
            ClientKind::Fifo(pipe) => {
                let usage = self.compile_queue.as_ref().map(|queue| {
                    let mut usage = queue.start_token();
                    usage.acquired();
                    usage
                });
                Ok(CommandToken {
                    _usage: usage,
                    token: Acquired {
                        _token: None,
                        pipe: Some(pipe.clone()),
                    },
                })
            }
            ClientKind::Pool(pool) => pool.acquire_inner(self.compile_queue.as_ref()).await,
        }
    }

    /// Whether this is the build's own jobserver, opened by path (see
    /// [`Client::from_env_fifo`]), rather than a private pool.
    pub fn is_fifo(&self) -> bool {
        matches!(self.kind, ClientKind::Fifo(_))
    }

    /// Returns a future that represents an acquired jobserver token.
    ///
    /// This should be invoked before any "work" is spawned (for whatever the
    /// definition of "work" is) to ensure that the system is properly
    /// rate-limiting itself.
    pub async fn acquire(&self) -> Result<Acquired> {
        match &self.kind {
            ClientKind::Fifo(pipe) => Ok(Acquired {
                _token: None,
                pipe: Some(pipe.clone()),
            }),
            ClientKind::Pool(pool) => Ok(pool.acquire_inner(None).await?.token),
        }
    }
}

/// Whether the jobserver in our environment, as `jobserver::Client::from_env`
/// would pick it, is opened by path.
fn env_names_fifo() -> bool {
    ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"]
        .iter()
        .find_map(std::env::var_os)
        .is_some_and(|flags| flags.to_str().is_some_and(names_fifo))
}

fn names_fifo(flags: &str) -> bool {
    flags
        .split_ascii_whitespace()
        .any(|arg| arg.starts_with("--jobserver-auth=fifo:"))
}

#[cfg(test)]
mod test {
    use super::*;

    #[cfg(target_os = "linux")]
    impl Pool {
        pub(crate) fn push_after_subscribe(&self, hook: impl FnOnce() + Send + 'static) {
            self.hooks
                .lock()
                .unwrap()
                .after_subscribe
                .get_or_insert_with(std::collections::VecDeque::new)
                .push_back(Box::new(hook));
        }

        pub(crate) fn push_after_snapshot(&self, hook: impl FnOnce() + Send + 'static) {
            self.hooks
                .lock()
                .unwrap()
                .after_snapshot
                .get_or_insert_with(std::collections::VecDeque::new)
                .push_back(Box::new(hook));
        }

        pub(crate) fn push_before_commit(
            &self,
            entered: oneshot::Sender<()>,
            gate: oneshot::Receiver<()>,
        ) {
            self.hooks
                .lock()
                .unwrap()
                .before_commit
                .get_or_insert_with(std::collections::VecDeque::new)
                .push_back((entered, gate));
        }

        pub(crate) fn set_fail_candidate(&self, fail: bool) {
            self.hooks.lock().unwrap().fail_candidate = fail;
        }

        pub(crate) fn set_candidate_gate(
            &self,
            entered: oneshot::Sender<()>,
            gate: std_mpsc::Receiver<()>,
        ) {
            let mut hooks = self.hooks.lock().unwrap();
            hooks.candidate_entered = Some(entered);
            hooks.candidate_gate = Some(gate);
        }

        pub(crate) fn set_next_helper_blocker(&self, gate: std_mpsc::Receiver<()>) {
            self.hooks.lock().unwrap().next_helper_blocker = Some(gate);
        }

        pub(crate) fn current_pipe_for_test(&self) -> Arc<Pipe> {
            self.current_pipe()
        }

        pub(crate) fn take_acquire_passes(&self) -> u64 {
            self.acquire_passes.swap(0, Ordering::Relaxed)
        }

        pub(crate) fn force_swap_for_test(
            &self,
        ) -> std::result::Result<SwapPublished, &'static str> {
            let expected_id = self.current.lock().unwrap().id;
            let candidate = self
                .build_candidate()
                .map_err(|_| "build_candidate failed")?;
            self.try_publish_swap(expected_id, candidate, |_| true)
                .map_err(|_| "swap rejected")
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_until_available(pipe: &Pipe, expected: usize) {
        let deadline = StdInstant::now() + Duration::from_secs(2);
        while pipe.available().unwrap() != expected && StdInstant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pipe.available().unwrap(), expected);
    }

    #[test]
    fn fifo_jobserver_flags() {
        assert!(names_fifo("-j8 --jobserver-auth=fifo:/tmp/GMfifo1234"));
        assert!(names_fifo(" -j --jobserver-auth=fifo:/dev/steve"));
        assert!(!names_fifo("-j8 --jobserver-fds=3,4 --jobserver-auth=3,4"));
        assert!(!names_fifo("-j8"));
        assert!(!names_fifo(""));
    }

    #[test]
    fn private_pool_is_not_fifo() {
        assert!(!Client::new_num(1).is_fifo());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokens_in_use_includes_tokens_taken_by_other_jobserver_clients() {
        let client = Client::new_num(2).with_compile_queue();
        let queue = client.compile_queue().unwrap();
        assert_eq!(queue.snapshot().tokens_in_use, Some(0));
        let other_client = client.pool().unwrap().current_pipe().inner.clone();
        let external_token = other_client.acquire().unwrap();
        let token = client.acquire_for_command().await.unwrap();
        let stats = queue.snapshot();
        assert_eq!(stats.running, 1);
        assert_eq!(stats.tokens_in_use, Some(2));
        drop(external_token);
        assert_eq!(queue.snapshot().tokens_in_use, Some(1));
        drop(token);
        assert_eq!(queue.snapshot().tokens_in_use, Some(0));
    }

    /// The worker-01 stall, at the scale of one token: a compiler that takes
    /// an extra token from our pipe, as rustc does for codegen, and is killed
    /// (`kill_on_drop` when its client goes away, or the OOM killer) never
    /// gives it back. A compiler that exits normally does.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn token_held_by_a_killed_compiler_is_lost() {
        use std::os::unix::process::ExitStatusExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        async fn spawn_holder(
            token: &CommandToken,
        ) -> (tokio::process::Child, tokio::process::ChildStdin) {
            let mut cmd = std::process::Command::new("bash");
            // Gives the token back on a line or on EOF, so only a kill loses it.
            cmd.env("LC_ALL", "C").arg("-c").arg(
                r#"auth=${CARGO_MAKEFLAGS##*--jobserver-auth=}; auth=${auth%% *}
                   IFS= read -r -n1 -u "${auth%,*}" token || exit 1
                   echo taken
                   read -r _
                   printf %s "$token" >&"${auth#*,}""#,
            );
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            token.configure(&mut cmd);
            let mut child = tokio::process::Command::from(cmd)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut line = String::new();
            tokio::io::BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .await
                .unwrap();
            assert_eq!(line, "taken\n");
            let stdin = child.stdin.take().unwrap();
            (child, stdin)
        }

        let client = Client::new_num(2).with_compile_queue();
        let queue = client.compile_queue().unwrap();

        let token = client.acquire_for_command().await.unwrap();
        let (mut child, mut stdin) = spawn_holder(&token).await;
        assert_eq!(queue.snapshot().tokens_in_use, Some(2));
        stdin.write_all(b"\n").await.unwrap();
        assert!(child.wait().await.unwrap().success());
        drop(token);
        assert_eq!(queue.snapshot().tokens_in_use, Some(0));

        let token = client.acquire_for_command().await.unwrap();
        let (mut child, stdin) = spawn_holder(&token).await;
        child.start_kill().unwrap();
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
        drop((stdin, token));
        let stats = queue.snapshot();
        assert_eq!((stats.running, stats.tokens_in_use), (0, Some(1)));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_waiter_swap_updates_watch_and_acquires_in_one_pass() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();

        // Swap with zero subscribed waiters (the idle-deficit case).
        let published = pool.force_swap_for_test().unwrap();
        assert_eq!(published.new_id, 1);
        assert_eq!(*pool.id_tx.borrow(), 1);
        assert_eq!(pool.current.lock().unwrap().id, 1);

        // Free pool: subsequent acquisition must succeed in a single pass.
        pool.take_acquire_passes();
        let first = client.acquire_for_command().await.unwrap();
        assert_eq!(pool.take_acquire_passes(), 1);

        // Saturated pool: a queued waiter must also stay on a single pass and
        // be admitted immediately when the held token drops.
        let waiter = tokio::spawn({
            let client = client.clone();
            async move { client.acquire_for_command().await.unwrap() }
        });
        while queue.snapshot().token_wait.active == 0 {
            tokio::task::yield_now().await;
        }
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("queued waiter livelocked after no-waiter swap")
            .unwrap();
        assert_eq!(pool.take_acquire_passes(), 1);
        drop(second);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn admission_race_swap_between_subscribe_and_snapshot() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let old_pipe = pool.current_pipe_for_test();

        let pool_for_hook = pool.clone();
        pool.push_after_subscribe(move || {
            let published = pool_for_hook.force_swap_for_test().unwrap();
            assert_eq!(published.new_id, 1);
        });

        let token = client.acquire_for_command().await.unwrap();
        let stats = queue.snapshot();
        assert_eq!(stats.running, 1);
        assert_eq!(stats.token_wait.active, 0);
        assert_eq!(stats.pool_swaps, Some(1));
        wait_until_available(&old_pipe, 1);
        assert_eq!(pool.current_pipe_for_test().available().unwrap(), 0);
        assert!(Arc::ptr_eq(
            token.token.pipe.as_ref().unwrap(),
            &pool.current_pipe_for_test()
        ));
        drop(token);
        assert_eq!(pool.current_pipe_for_test().available().unwrap(), 1);
        assert_eq!(queue.snapshot().running, 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn admission_race_token_and_swap_ready_at_once() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let old_pipe = pool.current_pipe_for_test();

        // Pin the exact path where `myrx` already received gen 0's token and is
        // parked right before commit, then publish gen 1.
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        pool.push_before_commit(entered_tx, release_rx);

        let acquire = tokio::spawn({
            let client = client.clone();
            async move { client.acquire_for_command().await.unwrap() }
        });
        entered_rx.await.unwrap();
        assert_eq!(old_pipe.available().unwrap(), 0);
        assert_eq!(queue.snapshot().token_wait.active, 1);
        assert_eq!(queue.snapshot().running, 0);

        let published = pool.force_swap_for_test().unwrap();
        assert_eq!(published.new_id, 1);
        // Unpark commit: waiter must reject the stale gen-0 token, return it to
        // `old_pipe`, keep the same `TokenUse`, and admit on gen 1.
        let _ = release_tx.send(());

        let token = acquire.await.unwrap();
        wait_until_available(&old_pipe, 1);
        assert_eq!(pool.current_pipe_for_test().available().unwrap(), 0);
        assert!(Arc::ptr_eq(
            token.token.pipe.as_ref().unwrap(),
            &pool.current_pipe_for_test()
        ));
        let stats = queue.snapshot();
        assert_eq!(stats.running, 1);
        assert_eq!(stats.token_wait.active, 0);
        drop(token);
        assert_eq!(queue.snapshot().running, 0);
        assert_eq!(pool.current_pipe_for_test().available().unwrap(), 1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn admission_race_cancel_after_delivery_returns_token_and_records_wait() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let pipe = pool.current_pipe_for_test();

        let (entered_tx, entered_rx) = oneshot::channel();
        let (_release_tx, release_rx) = oneshot::channel();
        pool.push_before_commit(entered_tx, release_rx);

        let started = tokio::time::Instant::now();
        let acquire = tokio::spawn({
            let client = client.clone();
            async move { client.acquire_for_command().await.unwrap() }
        });
        entered_rx.await.unwrap();
        assert_eq!(pipe.available().unwrap(), 0);
        tokio::time::advance(Duration::from_secs(12)).await;
        let expected_wait = started.elapsed();
        acquire.abort();
        let _ = acquire.await;

        wait_until_available(&pipe, 1);
        let stats = queue.snapshot();
        assert_eq!(stats.running, 0);
        assert_eq!(stats.token_wait.active, 0);
        assert_eq!(stats.token_waits_over_10s, 1);
        assert!(expected_wait >= Duration::from_secs(12));
        assert_eq!(stats.token_wait.max, expected_wait);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn retirement_repeated_swaps_with_empty_old_pipe() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let mut leaked = Vec::new();
        for expected in 1..=6 {
            // Drain the current pipe and park a waiter so the old helper is
            // actively blocked in `read()` when retired.
            let pipe = pool.current_pipe_for_test();
            leaked.push(pipe.inner.acquire().unwrap());
            assert_eq!(pipe.available().unwrap(), 0);
            let waiter = tokio::spawn({
                let client = client.clone();
                async move { client.acquire_for_command().await.unwrap() }
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            let published = pool.force_swap_for_test().unwrap();
            assert_eq!(published.new_id, expected);
            let token = waiter.await.unwrap();
            drop(token);
        }
        // All 6 helpers should finish retiring cleanly.
        let deadline = StdInstant::now() + Duration::from_secs(5);
        while pool.retire_pending_count() > 0 && StdInstant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(pool.retire_pending_count(), 0);
        assert_eq!(pool.pool_swaps(), Some(6));
        drop(leaked);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn admission_race_swap_between_snapshot_and_select() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let old_pipe = pool.current_pipe_for_test();

        let pool_for_hook = pool.clone();
        pool.push_after_snapshot(move || {
            let published = pool_for_hook.force_swap_for_test().unwrap();
            assert_eq!(published.new_id, 1);
        });

        let token = client.acquire_for_command().await.unwrap();
        let stats = queue.snapshot();
        assert_eq!(stats.running, 1);
        assert_eq!(stats.token_wait.active, 0);
        assert_eq!(stats.pool_swaps, Some(1));
        assert!(Arc::ptr_eq(
            token.token.pipe.as_ref().unwrap(),
            &pool.current_pipe_for_test()
        ));
        // Any token the old helper may have grabbed for the dropped request must
        // be returned to `old_pipe` once the old helper retires.
        wait_until_available(&old_pipe, 1);
        drop(token);
        assert_eq!(pool.current_pipe_for_test().available().unwrap(), 1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn retirement_stuck_helpers_count_and_stop_at_cap() {
        let client = Client::new_num(1).with_compile_queue();
        let pool = client.pool().unwrap();
        let mut unblocks = Vec::new();

        // Install the first blockable helper on generation 1 and let gen 0's
        // normal helper retire cleanly first.
        let (tx0, rx0) = std_mpsc::channel::<()>();
        unblocks.push(tx0);
        pool.set_next_helper_blocker(rx0);
        pool.force_swap_for_test().unwrap();
        let deadline = StdInstant::now() + Duration::from_secs(2);
        while pool.retire_pending_count() > 0 && StdInstant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(pool.retire_pending_count(), 0);

        // Retire `MAX_RETIRE_PENDING` consecutive stuck helpers (each blocked
        // inside its callback, where `SIGUSR1` cannot unblock it).
        for step in 0..MAX_RETIRE_PENDING {
            let stuck_waiter = tokio::spawn({
                let client = client.clone();
                async move { client.acquire_for_command().await }
            });
            let pipe = pool.current_pipe_for_test();
            let deadline = StdInstant::now() + Duration::from_secs(2);
            while pipe.available().unwrap() != 0 && StdInstant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(pipe.available().unwrap(), 0);
            // Cancel the waiter before installing the next blockable helper so
            // it does not immediately block on the next generation too.
            stuck_waiter.abort();
            let _ = stuck_waiter.await;

            if step + 1 < MAX_RETIRE_PENDING {
                let (tx, rx) = std_mpsc::channel::<()>();
                unblocks.push(tx);
                pool.set_next_helper_blocker(rx);
            }
            pool.force_swap_for_test().unwrap();
            assert_eq!(pool.retire_pending_count(), step + 1);
        }

        // Wait past one helper join timeout (~1 s) to verify a stuck helper
        // stays counted as pending after `HelperThread::drop` gives up.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(pool.retire_pending_count(), MAX_RETIRE_PENDING);
        // Further swaps must now be refused, while the current pool still works.
        assert!(pool.force_swap_for_test().is_err());
        pool.take_acquire_passes();
        let token = client.acquire_for_command().await.unwrap();
        assert_eq!(pool.take_acquire_passes(), 1);
        drop(token);

        // Release all blocked helpers so test threads exit cleanly.
        drop(unblocks);
        let deadline = StdInstant::now() + Duration::from_secs(5);
        while pool.retire_pending_count() > 0 && StdInstant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(pool.retire_pending_count(), 0);
    }
}
