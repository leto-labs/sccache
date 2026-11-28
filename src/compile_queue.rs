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

use crate::config::JobserverSwap;
use crate::jobserver::{MAX_RETIRE_PENDING, Pool};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time::Instant;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileQueueStats {
    pub token_capacity: usize,
    pub running: usize,
    pub token_wait: WaitStats,
    pub token_waits_over_10s: u64,
    pub stages: BTreeMap<CompileStage, WaitStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in_use: Option<usize>,
    /// How long compiles have waited with none running. Older daemons do not
    /// report it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled_for: Option<Duration>,
    /// Number of times the daemon swapped its private jobserver pipe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_swaps: Option<u64>,
    /// Retired helper threads that have not yet confirmed exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retire_pending: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WaitStats {
    pub total: Duration,
    pub max: Duration,
    pub active: usize,
    pub oldest: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompileStage {
    RequestQueue,
    CompilerSetup,
    TaskSetup,
    Hashing,
    CacheLookup,
    PendingCacheWrite,
    MissLog,
    DistSetup,
}

impl CompileStage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::RequestQueue => "Request queue",
            Self::CompilerSetup => "Compiler setup",
            Self::TaskSetup => "Compile task setup",
            Self::Hashing => "Hashing/preprocessing",
            Self::CacheLookup => "Cache lookup/restore",
            Self::PendingCacheWrite => "Pending cache write",
            Self::MissLog => "Rust miss logging",
            Self::DistSetup => "Distributed job setup",
        }
    }
}

#[derive(Default)]
struct Timing {
    total: Duration,
    max: Duration,
    active: usize,
    // Several waits can start in the same clock tick.
    starts: BTreeMap<Instant, usize>,
}

impl Timing {
    fn start(&mut self) -> Instant {
        let start = Instant::now();
        *self.starts.entry(start).or_default() += 1;
        self.active += 1;
        start
    }

    fn finish(&mut self, start: Instant, elapsed: Duration) {
        let count = self.starts.get_mut(&start).unwrap();
        *count -= 1;
        if *count == 0 {
            self.starts.remove(&start);
        }
        self.active -= 1;
        self.total += elapsed;
        self.max = self.max.max(elapsed);
    }

    fn snapshot(&self, now: Instant) -> WaitStats {
        WaitStats {
            total: self.total,
            max: self.max,
            active: self.active,
            oldest: self
                .starts
                .first_key_value()
                .map_or(Duration::ZERO, |(&start, _)| now - start),
        }
    }
}

#[derive(Default)]
struct QueueState {
    token_wait: Timing,
    token_waits_over_10s: u64,
    running: usize,
    // Incremented whenever a token wait starts or a compile begins running so
    // idle-deficit detection resets if any activity occurs between 5 s ticks.
    busy_seq: u64,
    last_warning: Option<Instant>,
    stages: BTreeMap<CompileStage, Timing>,
    last_stage_warning: BTreeMap<CompileStage, Instant>,
    stalled_since: Option<Instant>,
    last_stall_error: Option<Instant>,
}

impl QueueState {
    /// Called whenever `running` or the number waiting changes, so a stall is
    /// one unbroken stretch with waiters and nothing running.
    fn note_transition(&mut self, now: Instant) {
        if self.running == 0 && self.token_wait.active > 0 {
            self.stalled_since.get_or_insert(now);
        } else {
            self.stalled_since = None;
        }
    }

    fn reserve_stage_warning(&mut self, stage: CompileStage, now: Instant) -> bool {
        if self
            .last_stage_warning
            .get(&stage)
            .is_some_and(|&last| now - last < Duration::from_secs(60))
        {
            return false;
        }
        self.last_stage_warning.insert(stage, now);
        true
    }

    fn reserve_warning(&mut self, now: Instant, elapsed: Duration) -> Option<(usize, usize)> {
        if elapsed > TokenUse::WARN_AFTER
            && self
                .last_warning
                .is_none_or(|last| now - last >= Duration::from_secs(60))
        {
            self.last_warning = Some(now);
            Some((self.token_wait.active, self.running))
        } else {
            None
        }
    }

    fn finish_token_wait(&mut self, start: Instant) -> Option<(usize, usize)> {
        let now = Instant::now();
        let elapsed = now - start;
        let warning = self.reserve_warning(now, elapsed);
        self.token_wait.finish(start, elapsed);
        self.token_waits_over_10s += u64::from(elapsed > Duration::from_secs(10));
        self.note_transition(now);
        warning
    }
}

/// How long compiles may wait with none running before that is logged as an error.
pub(crate) const STALL_ERROR_AFTER: Duration = Duration::from_secs(60);

/// How long an empty pipe with waiters and nothing running must persist within
/// one generation before swapping the pool.
pub(crate) const HARD_STALL_SWAP_AFTER: Duration = Duration::from_secs(120);

/// How long an idle daemon missing at least a quarter of its tokens (by the
/// same count) must persist within one generation before swapping the pool.
pub(crate) const IDLE_DEFICIT_SWAP_AFTER: Duration = Duration::from_secs(600);

/// Backoff after a replacement pool creation fails.
pub(crate) const SWAP_FAILURE_BACKOFF: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwapTrigger {
    HardStall { stalled_since: Option<Instant> },
    IdleDeficit { deficit: usize, busy_seq: u64 },
}

impl SwapTrigger {
    fn label(self) -> &'static str {
        match self {
            Self::HardStall { .. } => "hard stall",
            Self::IdleDeficit { .. } => "idle deficit",
        }
    }
}

#[derive(Default)]
struct SwapWatchState {
    hard_stall: Option<(u64, Option<Instant>, Instant)>,
    idle_deficit: Option<(u64, u64, usize, Instant)>,
    backoff_until: Option<Instant>,
    retire_cap_logged: bool,
}

fn log_token_warning(warning: Option<(usize, usize)>) {
    if let Some((waiting, running)) = warning {
        warn!("Compiler token wait exceeded 30 s ({waiting} waiting, {running} running)");
    }
}

pub(crate) struct CompileQueue {
    capacity: usize,
    pool: Option<Arc<Pool>>,
    swap_policy: JobserverSwap,
    state: Mutex<QueueState>,
}

impl CompileQueue {
    pub(crate) fn new(
        capacity: usize,
        pool: Option<Arc<Pool>>,
        swap_policy: JobserverSwap,
    ) -> Self {
        Self {
            capacity,
            pool,
            swap_policy,
            state: Mutex::default(),
        }
    }

    pub(crate) fn snapshot(&self) -> CompileQueueStats {
        let build =
            |tokens_in_use: Option<usize>, pool_swaps: Option<u64>, retire_pending: Option<u64>| {
                let state = self.state.lock().unwrap();
                let now = Instant::now();
                CompileQueueStats {
                    token_capacity: self.capacity,
                    tokens_in_use,
                    // The wire carries stall and swap fields after the gauge, so
                    // they are only populated when the gauge is present.
                    stalled_for: tokens_in_use.map(|_| {
                        state
                            .stalled_since
                            .map_or(Duration::ZERO, |since| now - since)
                    }),
                    pool_swaps: tokens_in_use.and(pool_swaps),
                    retire_pending: tokens_in_use.and(retire_pending),
                    running: state.running,
                    token_wait: state.token_wait.snapshot(now),
                    token_waits_over_10s: state.token_waits_over_10s,
                    stages: state
                        .stages
                        .iter()
                        .map(|(&stage, timing)| (stage, timing.snapshot(now)))
                        .collect(),
                }
            };
        if let Some(pool) = &self.pool {
            pool.with_current(|_, available| {
                let tokens_in_use = available.map(|avail| self.capacity.saturating_sub(avail));
                build(tokens_in_use, pool.pool_swaps(), pool.retire_pending())
            })
        } else {
            build(None, None, None)
        }
    }

    pub(crate) fn start_stage(self: &Arc<Self>, stage: CompileStage) -> StageGuard {
        let start = self
            .state
            .lock()
            .unwrap()
            .stages
            .entry(stage)
            .or_default()
            .start();
        StageGuard {
            queue: self.clone(),
            stage,
            start,
        }
    }

    pub(crate) fn start_token(self: &Arc<Self>) -> TokenUse {
        let mut state = self.state.lock().unwrap();
        state.busy_seq = state.busy_seq.wrapping_add(1);
        let start = state.token_wait.start();
        state.note_transition(start);
        TokenUse {
            queue: self.clone(),
            waiting_since: Some(start),
        }
    }

    /// Logs an error, at most once a minute, while compiles have waited
    /// longer than [`STALL_ERROR_AFTER`] with none running.
    fn check_stall(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        let Some(since) = state.stalled_since else {
            return;
        };
        let stalled = now - since;
        if stalled >= STALL_ERROR_AFTER
            && state
                .last_stall_error
                .is_none_or(|last| now - last >= STALL_ERROR_AFTER)
        {
            state.last_stall_error = Some(now);
            let waiting = state.token_wait.active;
            drop(state);
            error!(
                "{waiting} compiles waiting for a jobserver token with none running for {}s",
                stalled.as_secs()
            );
        }
    }

    fn is_idle_deficit(&self, in_use: usize) -> bool {
        in_use > 0 && in_use.saturating_mul(4) >= self.capacity
    }

    fn evaluate_swap_trigger(
        &self,
        pool: &Pool,
        now: Instant,
        watch: &mut SwapWatchState,
    ) -> Option<(u64, SwapTrigger)> {
        let retire_pending = pool.retire_pending_count();
        if retire_pending >= MAX_RETIRE_PENDING {
            watch.hard_stall = None;
            watch.idle_deficit = None;
            if !watch.retire_cap_logged {
                watch.retire_cap_logged = true;
                error!(
                    "disabling jobserver pool swaps: {retire_pending} helper retirements pending \
                     (limit {MAX_RETIRE_PENDING})"
                );
            }
            return None;
        }
        watch.retire_cap_logged = false;

        let (gen_id, available, running, waiting, stalled_since, busy_seq) =
            pool.with_current(|gen_id, available| {
                let state = self.state.lock().unwrap();
                (
                    gen_id,
                    available,
                    state.running,
                    state.token_wait.active,
                    state.stalled_since,
                    state.busy_seq,
                )
            });

        let Some(available) = available else {
            watch.hard_stall = None;
            watch.idle_deficit = None;
            return None;
        };
        let in_use = self.capacity.saturating_sub(available);

        if self.swap_policy.on_stall() && available == 0 && running == 0 && waiting > 0 {
            match watch.hard_stall {
                Some((id, prev_stalled, _)) if id == gen_id && prev_stalled == stalled_since => {}
                _ => watch.hard_stall = Some((gen_id, stalled_since, now)),
            }
        } else {
            watch.hard_stall = None;
        }

        if self.swap_policy.on_idle()
            && running == 0
            && waiting == 0
            && self.is_idle_deficit(in_use)
        {
            match watch.idle_deficit {
                Some((id, prev_seq, prev_in_use, _))
                    if id == gen_id && prev_seq == busy_seq && prev_in_use == in_use => {}
                _ => watch.idle_deficit = Some((gen_id, busy_seq, in_use, now)),
            }
        } else {
            watch.idle_deficit = None;
        }

        if watch.backoff_until.is_some_and(|until| now < until) {
            return None;
        }

        if let Some((id, prev_stalled, since)) = watch.hard_stall
            && id == gen_id
            && now - since >= HARD_STALL_SWAP_AFTER
        {
            return Some((
                gen_id,
                SwapTrigger::HardStall {
                    stalled_since: prev_stalled,
                },
            ));
        }

        if let Some((id, prev_seq, deficit, since)) = watch.idle_deficit
            && id == gen_id
            && now - since >= IDLE_DEFICIT_SWAP_AFTER
        {
            return Some((
                gen_id,
                SwapTrigger::IdleDeficit {
                    deficit,
                    busy_seq: prev_seq,
                },
            ));
        }

        None
    }

    async fn maybe_swap_pool(&self, now: Instant, watch: &mut SwapWatchState) {
        if !Pool::supports_swap() || self.swap_policy == JobserverSwap::Off {
            return;
        }
        let Some(pool) = &self.pool else {
            return;
        };
        let Some((snap_id, trigger)) = self.evaluate_swap_trigger(pool, now, watch) else {
            return;
        };

        let pool_for_build = pool.clone();
        let candidate =
            match tokio::task::spawn_blocking(move || pool_for_build.build_candidate()).await {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(err)) => {
                    error!(
                        "failed to create replacement jobserver pool: {err}; keeping current pool \
                     and backing off for {}s",
                        SWAP_FAILURE_BACKOFF.as_secs()
                    );
                    watch.backoff_until = Some(Instant::now() + SWAP_FAILURE_BACKOFF);
                    return;
                }
                Err(err) if err.is_cancelled() => {
                    debug!("jobserver pool creation task cancelled: {err}");
                    return;
                }
                Err(err) => {
                    error!("jobserver pool creation task failed: {err}");
                    watch.backoff_until = Some(Instant::now() + SWAP_FAILURE_BACKOFF);
                    return;
                }
            };

        let publish = pool.try_publish_swap(snap_id, candidate, |available| {
            let Some(available) = available else {
                return false;
            };
            let state = self.state.lock().unwrap();
            match trigger {
                SwapTrigger::HardStall { stalled_since } => {
                    available == 0
                        && state.running == 0
                        && state.token_wait.active > 0
                        && state.stalled_since == stalled_since
                }
                SwapTrigger::IdleDeficit { deficit, busy_seq } => {
                    let in_use = self.capacity.saturating_sub(available);
                    state.running == 0
                        && state.token_wait.active == 0
                        && state.busy_seq == busy_seq
                        && in_use == deficit
                        && self.is_idle_deficit(in_use)
                }
            }
        });

        match publish {
            Ok(published) => {
                watch.hard_stall = None;
                watch.idle_deficit = None;
                watch.backoff_until = None;
                let old_in_use = published
                    .old_in_use
                    .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
                error!(
                    "swapped jobserver pool to generation {} ({}, old tokens_in_use={}/{}, \
                     generation_age={}s)",
                    published.new_id,
                    trigger.label(),
                    old_in_use,
                    self.capacity,
                    published.old_age.as_secs()
                );
            }
            Err(unused) => {
                watch.hard_stall = None;
                watch.idle_deficit = None;
                let _ = tokio::task::spawn_blocking(move || drop(unused)).await;
            }
        }
    }

    /// Watches the compile queue every 5 s: logs errors once a stall lasts
    /// over [`STALL_ERROR_AFTER`], and on Linux swaps to a fresh private
    /// jobserver pipe when killed compilers deplete the pool. Never returns.
    pub(crate) async fn log_stalls(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut watch = SwapWatchState::default();
        loop {
            tick.tick().await;
            let now = Instant::now();
            self.check_stall(now);
            self.maybe_swap_pool(now, &mut watch).await;
        }
    }
}

pub(crate) struct StageGuard {
    queue: Arc<CompileQueue>,
    stage: CompileStage,
    start: Instant,
}

impl StageGuard {
    pub(crate) fn warn_if_slow(self, crate_name: &str, work: &StageWork, hash_work: &StageWork) {
        let threshold = match self.stage {
            CompileStage::Hashing => Duration::from_secs(30),
            CompileStage::CacheLookup => Duration::from_secs(10),
            _ => return,
        };
        let now = Instant::now();
        let elapsed = now - self.start;
        if elapsed <= threshold || work.0.is_none() {
            return;
        }
        let stage = self.stage;
        let warn = self
            .queue
            .state
            .lock()
            .unwrap()
            .reserve_stage_warning(stage, now);
        drop(self);
        if warn {
            let work = work.0.as_ref().unwrap();
            let (files, bytes) = hash_work.0.as_ref().map_or((0, 0), |hash_work| {
                (hash_work.files.load(Relaxed), hash_work.bytes.load(Relaxed))
            });
            let wait = Duration::from_nanos(work.max_blocking_wait.load(Relaxed));
            let run = Duration::from_nanos(work.total_blocking_work.load(Relaxed));
            let pending = work.pending.load(Relaxed);
            warn!(
                "Slow {stage:?} for Rust crate {crate_name}: {elapsed:.3?}, \
                 input_files={files}, bytes_hashed={bytes}, blocking_wait_max={wait:.3?}, \
                 blocking_work_total={run:.3?}, blocking_tasks_pending={pending}"
            );
        }
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        let elapsed = self.start.elapsed();
        state
            .stages
            .get_mut(&self.stage)
            .unwrap()
            .finish(self.start, elapsed);
    }
}

#[derive(Default)]
struct WorkCounters {
    files: AtomicU64,
    bytes: AtomicU64,
    max_blocking_wait: AtomicU64,
    total_blocking_work: AtomicU64,
    pending: AtomicU64,
}

#[derive(Clone, Default)]
pub(crate) struct StageWork(Option<Arc<WorkCounters>>);

tokio::task_local! {
    static STAGE_WORK: StageWork;
}

impl StageWork {
    pub(crate) fn new(enabled: bool) -> Self {
        Self(enabled.then(Arc::default))
    }

    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        if self.0.is_some() {
            STAGE_WORK.scope(self.clone(), future).await
        } else {
            future.await
        }
    }

    pub(crate) fn record_file(&self, bytes: u64) {
        if let Some(work) = &self.0 {
            work.files.fetch_add(1, Relaxed);
            work.bytes.fetch_add(bytes, Relaxed);
        }
    }
}

pub(crate) fn spawn_blocking<T: Send + 'static>(
    pool: &Handle,
    task: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    spawn_blocking_with_work(pool, |_| task())
}

pub(crate) fn spawn_blocking_with_work<T: Send + 'static>(
    pool: &Handle,
    task: impl FnOnce(&StageWork) -> T + Send + 'static,
) -> JoinHandle<T> {
    let work = STAGE_WORK.try_with(Clone::clone).unwrap_or_default();
    let Some(counters) = &work.0 else {
        return pool.spawn_blocking(move || task(&work));
    };
    counters.pending.fetch_add(1, Relaxed);
    let queued = std::time::Instant::now();
    pool.spawn_blocking(move || {
        let started = std::time::Instant::now();
        let counters = work.0.as_ref().unwrap();
        counters
            .max_blocking_wait
            .fetch_max((started - queued).as_nanos() as u64, Relaxed);
        counters.pending.fetch_sub(1, Relaxed);
        let result = task(&work);
        counters
            .total_blocking_work
            .fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
        result
    })
}

pub(crate) struct TokenUse {
    queue: Arc<CompileQueue>,
    waiting_since: Option<Instant>,
}

impl TokenUse {
    const WARN_AFTER: Duration = Duration::from_secs(30);

    pub(crate) fn warning_deadline(&self) -> Instant {
        self.waiting_since.unwrap() + Self::WARN_AFTER + Duration::from_nanos(1)
    }

    pub(crate) fn warn_if_slow(&self) {
        let mut state = self.queue.state.lock().unwrap();
        let now = Instant::now();
        let warning = state.reserve_warning(now, now - self.waiting_since.unwrap());
        drop(state);
        log_token_warning(warning);
    }

    pub(crate) fn acquired(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        state.busy_seq = state.busy_seq.wrapping_add(1);
        let warning = state.finish_token_wait(self.waiting_since.take().unwrap());
        state.running += 1;
        state.note_transition(Instant::now());
        drop(state);
        log_token_warning(warning);
    }
}

impl Drop for TokenUse {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap();
        if let Some(start) = self.waiting_since {
            let warning = state.finish_token_wait(start);
            drop(state);
            log_token_warning(warning);
        } else {
            state.running -= 1;
            state.note_transition(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobserver::Client;

    #[tokio::test(start_paused = true)]
    async fn slow_stage_warnings_respect_thresholds_and_rate_limits() {
        let queue = Arc::new(CompileQueue::new(1, None, JobserverSwap::Off));
        let work = StageWork::new(true);
        let hashing = queue.start_stage(CompileStage::Hashing);
        let lookup = queue.start_stage(CompileStage::CacheLookup);
        tokio::time::advance(Duration::from_secs(10)).await;
        lookup.warn_if_slow("example", &work, &work);
        assert!(queue.state.lock().unwrap().last_stage_warning.is_empty());

        let lookup = queue.start_stage(CompileStage::CacheLookup);
        tokio::time::advance(Duration::from_secs(20)).await;
        hashing.warn_if_slow("example", &work, &work);
        lookup.warn_if_slow("example", &work, &work);
        assert!(
            !queue
                .state
                .lock()
                .unwrap()
                .last_stage_warning
                .contains_key(&CompileStage::Hashing)
        );
        let first_lookup_warning =
            queue.state.lock().unwrap().last_stage_warning[&CompileStage::CacheLookup];

        let hashing = queue.start_stage(CompileStage::Hashing);
        let lookup = queue.start_stage(CompileStage::CacheLookup);
        tokio::time::advance(Duration::from_secs(31)).await;
        hashing.warn_if_slow("example", &work, &work);
        lookup.warn_if_slow("example", &work, &work);
        assert_eq!(
            queue.state.lock().unwrap().last_stage_warning[&CompileStage::Hashing],
            Instant::now()
        );
        assert_eq!(
            queue.state.lock().unwrap().last_stage_warning[&CompileStage::CacheLookup],
            first_lookup_warning
        );

        let lookup = queue.start_stage(CompileStage::CacheLookup);
        tokio::time::advance(Duration::from_secs(29)).await;
        lookup.warn_if_slow("example", &work, &work);
        assert_eq!(
            queue.state.lock().unwrap().last_stage_warning[&CompileStage::CacheLookup],
            Instant::now()
        );
    }

    #[test]
    fn blocking_work_measures_queue_delay_and_hash_input_bytes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let worker = runtime.spawn_blocking(move || blocked.recv().unwrap());
        let work = StageWork::new(true);
        let counters = work.0.as_ref().unwrap();
        runtime.block_on(work.scope(async {
            let task = spawn_blocking_with_work(runtime.handle(), |work| work.record_file(128));
            assert_eq!(counters.pending.load(Relaxed), 1);
            release.send(()).unwrap();
            task.await.unwrap();
            worker.await.unwrap();
        }));
        assert_eq!(counters.pending.load(Relaxed), 0);
        assert_eq!(counters.files.load(Relaxed), 1);
        assert_eq!(counters.bytes.load(Relaxed), 128);
        assert!(counters.max_blocking_wait.load(Relaxed) > 0);
        assert!(counters.total_blocking_work.load(Relaxed) > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stall_is_one_unbroken_stretch_with_waiters_and_nothing_running() {
        assert_eq!(
            CompileQueue::new(1, None, JobserverSwap::Off)
                .snapshot()
                .stalled_for,
            None
        );
        let client = Client::new_num(1).with_compile_queue_swap(JobserverSwap::Off);
        let queue = client.compile_queue().unwrap();
        let gauged = cfg!(unix);
        let stalled_for = || {
            let since = queue.state.lock().unwrap().stalled_since;
            let stalled = since.map_or(Duration::ZERO, |since| Instant::now() - since);
            // The stats report the same, where the gauge is.
            if gauged {
                assert_eq!(queue.snapshot().stalled_for, Some(stalled));
            }
            stalled
        };
        assert_eq!(stalled_for(), Duration::ZERO);
        let waiter = queue.start_token();
        tokio::time::advance(Duration::from_secs(50)).await;
        assert_eq!(stalled_for(), Duration::from_secs(50));

        // A short compile between two watchdog ticks ends the stretch.
        let mut short = queue.start_token();
        short.acquired();
        assert_eq!(stalled_for(), Duration::ZERO);
        tokio::time::advance(Duration::from_secs(1)).await;
        drop(short);
        tokio::time::advance(Duration::from_secs(50)).await;
        assert_eq!(stalled_for(), Duration::from_secs(50));

        drop(waiter);
        assert_eq!(stalled_for(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn stall_error_is_logged_after_a_minute_and_at_most_once_a_minute() {
        let queue = Arc::new(CompileQueue::new(1, None, JobserverSwap::Off));
        let logger = tokio::spawn(queue.clone().log_stalls());
        let last_error = || queue.state.lock().unwrap().last_stall_error;
        let mut running = queue.start_token();
        running.acquired();
        let waiter = queue.start_token();
        tokio::time::sleep(STALL_ERROR_AFTER * 2).await;
        assert_eq!(last_error(), None);

        drop(running);
        let tick = Duration::from_secs(5);
        tokio::time::sleep(STALL_ERROR_AFTER - tick).await;
        assert_eq!(last_error(), None);
        tokio::time::sleep(tick * 2).await;
        let first = last_error().unwrap();
        tokio::time::sleep(STALL_ERROR_AFTER - tick * 2).await;
        assert_eq!(last_error(), Some(first));
        tokio::time::sleep(tick * 2).await;
        assert!(last_error().unwrap() >= first + STALL_ERROR_AFTER);
        drop(waiter);
        logger.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_token_wait_is_recorded_and_warnings_are_rate_limited() {
        let queue = Arc::new(CompileQueue::new(1, None, JobserverSwap::Off));
        let first = queue.start_token();
        let second = queue.start_token();

        tokio::time::advance(Duration::from_secs(30)).await;
        first.warn_if_slow();
        assert!(queue.state.lock().unwrap().last_warning.is_none());

        tokio::time::advance(Duration::from_secs(1)).await;
        first.warn_if_slow();
        let first_warning = queue.state.lock().unwrap().last_warning.unwrap();
        drop(first);
        let stats = queue.snapshot();
        assert_eq!(stats.token_wait.active, 1);
        assert_eq!(stats.token_wait.oldest, Duration::from_secs(31));
        assert_eq!(stats.token_wait.max, Duration::from_secs(31));
        assert_eq!(stats.token_waits_over_10s, 1);

        tokio::time::advance(Duration::from_secs(59)).await;
        second.warn_if_slow();
        assert_eq!(
            queue.state.lock().unwrap().last_warning,
            Some(first_warning)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        second.warn_if_slow();
        assert_eq!(
            queue.state.lock().unwrap().last_warning,
            Some(first_warning + Duration::from_secs(60))
        );
    }

    #[cfg(target_os = "linux")]
    async fn leak_one_token_via_killed_child(client: &Client) {
        use std::os::unix::process::ExitStatusExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let token = client.acquire_for_command().await.unwrap();
        let mut cmd = std::process::Command::new("bash");
        // Wait for "go" on stdin before reading a token from the pipe so we can
        // drop our admission token first and drain even the last pipe token.
        cmd.env("LC_ALL", "C").arg("-c").arg(
            r#"auth=${CARGO_MAKEFLAGS##*--jobserver-auth=}; auth=${auth%% *}
               read -r _
               IFS= read -r -n1 -u "${auth%,*}" token || exit 1
               echo taken
               read -r _"#,
        );
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        token.configure(&mut cmd);
        let mut child = tokio::process::Command::from(cmd)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // Return the daemon admission token so the child can take any remaining
        // pipe token (including the final one) before being SIGKILLed.
        drop(token);
        stdin.write_all(b"go\n").await.unwrap();
        let mut line = String::new();
        tokio::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line, "taken\n");
        child.start_kill().unwrap();
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
        drop(stdin);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn hard_stall_with_killed_children_recovers_by_swapping() {
        let client = Client::new_num(2).with_compile_queue_swap(JobserverSwap::Stall);
        let queue = client.compile_queue().unwrap();

        // Repeatedly leak tokens via real killed children until the pipe is
        // empty, before starting the paused-clock watchdog.
        leak_one_token_via_killed_child(&client).await;
        assert_eq!(queue.snapshot().tokens_in_use, Some(1));
        leak_one_token_via_killed_child(&client).await;
        assert_eq!(queue.snapshot().tokens_in_use, Some(2));
        assert_eq!(queue.snapshot().running, 0);

        let watchdog = tokio::spawn(queue.clone().log_stalls());
        let waiter = tokio::spawn({
            let client = client.clone();
            async move { client.acquire_for_command().await.unwrap() }
        });
        // Not swapped before 120 s.
        tokio::time::sleep(HARD_STALL_SWAP_AFTER - Duration::from_secs(10)).await;
        assert!(!waiter.is_finished());
        assert_eq!(queue.snapshot().pool_swaps, Some(0));

        // Past 120 s the watchdog swaps and the waiter is admitted.
        tokio::time::sleep(Duration::from_secs(20)).await;
        let token = waiter.await.unwrap();
        let stats = queue.snapshot();
        assert_eq!(stats.pool_swaps, Some(1));
        assert_eq!(stats.running, 1);
        assert_eq!(stats.tokens_in_use, Some(1));
        drop(token);
        assert_eq!(queue.snapshot().tokens_in_use, Some(0));

        watchdog.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn idle_deficit_swaps_at_idle_and_respects_policy_and_running_compiles() {
        // Capacity 4: losing 1 token is 25% (triggers idle deficit).
        let client = Client::new_num(4).with_compile_queue_swap(JobserverSwap::StallAndIdle);
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();

        leak_one_token_via_killed_child(&client).await;
        assert_eq!(queue.snapshot().tokens_in_use, Some(1));

        let watchdog = tokio::spawn(queue.clone().log_stalls());

        // While a compile is running, no swap happens even past 10 min.
        let running = client.acquire_for_command().await.unwrap();
        tokio::time::sleep(IDLE_DEFICIT_SWAP_AFTER + Duration::from_secs(30)).await;
        assert_eq!(queue.snapshot().pool_swaps, Some(0));
        drop(running);

        // Once idle, it waits a full 10 min with the same deficit count.
        tokio::time::sleep(IDLE_DEFICIT_SWAP_AFTER - Duration::from_secs(15)).await;
        assert_eq!(queue.snapshot().pool_swaps, Some(0));
        tokio::time::sleep(Duration::from_secs(25)).await;
        let stats = queue.snapshot();
        assert_eq!(stats.pool_swaps, Some(1));
        assert_eq!(stats.tokens_in_use, Some(0));
        // Acquiring after a no-waiter idle swap must succeed in one pass.
        pool.take_acquire_passes();
        let post_swap = client.acquire_for_command().await.unwrap();
        assert_eq!(pool.take_acquire_passes(), 1);
        drop(post_swap);
        watchdog.abort();

        // With `Stall` policy, idle deficit does not swap.
        let stall_only = Client::new_num(4).with_compile_queue_swap(JobserverSwap::Stall);
        let queue2 = stall_only.compile_queue().unwrap();
        leak_one_token_via_killed_child(&stall_only).await;
        let watchdog2 = tokio::spawn(queue2.clone().log_stalls());
        tokio::time::sleep(IDLE_DEFICIT_SWAP_AFTER + Duration::from_secs(60)).await;
        assert_eq!(queue2.snapshot().pool_swaps, Some(0));
        assert_eq!(queue2.snapshot().tokens_in_use, Some(1));
        watchdog2.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn creation_failure_keeps_old_pool_and_backs_off() {
        let client = Client::new_num(2).with_compile_queue_swap(JobserverSwap::Stall);
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let old_pipe = pool.current_pipe_for_test();
        pool.set_fail_candidate(true);

        leak_one_token_via_killed_child(&client).await;
        leak_one_token_via_killed_child(&client).await;

        let watchdog = tokio::spawn(queue.clone().log_stalls());
        let waiter = tokio::spawn({
            let client = client.clone();
            async move { client.acquire_for_command().await.unwrap() }
        });

        // Past 120 s creation fails, old pool stays, watchdog keeps running.
        tokio::time::sleep(HARD_STALL_SWAP_AFTER + Duration::from_secs(20)).await;
        assert!(!watchdog.is_finished());
        assert_eq!(queue.snapshot().pool_swaps, Some(0));
        assert!(Arc::ptr_eq(&pool.current_pipe_for_test(), &old_pipe));

        // Clear the failure; backoff prevents swapping until 10 min elapses.
        pool.set_fail_candidate(false);
        tokio::time::sleep(SWAP_FAILURE_BACKOFF - Duration::from_secs(30)).await;
        assert_eq!(queue.snapshot().pool_swaps, Some(0));

        // After backoff expires, the next tick swaps and unblocks the waiter.
        tokio::time::sleep(Duration::from_secs(60)).await;
        let token = waiter.await.unwrap();
        assert_eq!(queue.snapshot().pool_swaps, Some(1));
        assert!(!Arc::ptr_eq(&pool.current_pipe_for_test(), &old_pipe));
        drop(token);
        watchdog.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn shutdown_during_swap_preparation_leaves_old_pool_intact() {
        let client = Client::new_num(2).with_compile_queue_swap(JobserverSwap::Stall);
        let pool = client.pool().unwrap();
        let queue = client.compile_queue().unwrap();
        let old_pipe = pool.current_pipe_for_test();

        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        pool.set_candidate_gate(entered_tx, gate_rx);

        leak_one_token_via_killed_child(&client).await;
        leak_one_token_via_killed_child(&client).await;

        let waiter = queue.start_token();
        let watchdog = tokio::spawn(queue.clone().log_stalls());
        // Advance tick by tick until `build_candidate` starts on `spawn_blocking`.
        let mut entered = false;
        let mut entered_rx = entered_rx;
        for _ in 0..30 {
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            if entered_rx.try_recv().is_ok() {
                entered = true;
                break;
            }
        }
        if !entered {
            entered_rx.await.unwrap();
        }

        // Simulate server shutdown cancelling `log_stalls` mid-preparation.
        watchdog.abort();
        let _ = watchdog.await;
        drop(gate_tx);

        // Candidate is dropped off the runtime without publishing.
        assert_eq!(queue.snapshot().pool_swaps, Some(0));
        assert!(Arc::ptr_eq(&pool.current_pipe_for_test(), &old_pipe));
        drop(waiter);
    }
}
