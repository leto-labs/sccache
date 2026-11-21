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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompileQueueStats {
    pub token_capacity: usize,
    pub running: usize,
    pub token_wait: WaitStats,
    pub token_waits_over_10s: u64,
    pub stages: BTreeMap<CompileStage, WaitStats>,
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
}

impl QueueState {
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
    state: Mutex<QueueState>,
}

impl CompileQueue {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::default(),
        }
    }

    pub(crate) fn snapshot(&self) -> CompileQueueStats {
        let state = self.state.lock().unwrap();
        let now = Instant::now();
        CompileQueueStats {
            token_capacity: self.capacity,
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
    async fn cancelled_token_wait_is_recorded_and_warnings_are_rate_limited() {
        let queue = Arc::new(CompileQueue::new(1));
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
