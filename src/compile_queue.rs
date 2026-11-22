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
    last_warning: Option<Instant>,
    stages: BTreeMap<CompileStage, Timing>,
    last_stage_warning: BTreeMap<CompileStage, Instant>,
}

impl QueueState {
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
        warning
    }
}

fn log_token_warning(warning: Option<(usize, usize)>) {
    if let Some((waiting, running)) = warning {
        warn!("Compiler token wait exceeded 30 s ({waiting} waiting, {running} running)");
    }
}

pub(crate) struct CompileQueue {
    capacity: usize,
    jobserver: Option<jobserver::Client>,
    state: Mutex<QueueState>,
}

impl CompileQueue {
    pub(crate) fn new(capacity: usize, jobserver: Option<jobserver::Client>) -> Self {
        Self {
            capacity,
            jobserver,
            state: Mutex::default(),
        }
    }

    pub(crate) fn snapshot(&self) -> CompileQueueStats {
        let tokens_in_use = self
            .jobserver
            .as_ref()
            .and_then(|client| client.available().ok())
            .and_then(|available| self.capacity.checked_sub(available));
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        CompileQueueStats {
            token_capacity: self.capacity,
            tokens_in_use,
            running: state.running,
            token_wait: state.token_wait.snapshot(now),
            token_waits_over_10s: state.token_waits_over_10s,
            stages: state
                .stages
                .iter()
                .map(|(&stage, timing)| (stage, timing.snapshot(now)))
                .collect(),
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
        let start = self.state.lock().unwrap().token_wait.start();
        TokenUse {
            queue: self.clone(),
            waiting_since: Some(start),
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
        let warning = state.finish_token_wait(self.waiting_since.take().unwrap());
        state.running += 1;
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn slow_stage_warnings_respect_thresholds_and_rate_limits() {
        let queue = Arc::new(CompileQueue::new(1, None));
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
    async fn cancelled_token_wait_is_recorded_and_warnings_are_rate_limited() {
        let queue = Arc::new(CompileQueue::new(1, None));
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
}
