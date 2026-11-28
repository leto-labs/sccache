use std::collections::{HashMap, VecDeque};
use std::fs;
use std::time::Instant;

use serde::Serialize;

use crate::psi::{PsiAction, PsiGovernor, PsiSnapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    ReplyByte { unique: u64 },
    ReplyError { unique: u64, errno: i32 },
    NotifyPoll { kh: u64 },
}

#[derive(Clone, Debug)]
struct OwnerState {
    comm: String,
    cmdline: String,
    open_fhs: usize,
    held: usize,
    acquired_total: u64,
    released_total: u64,
}

#[derive(Clone, Debug)]
struct ReadWaiter {
    unique: u64,
    fh: u64,
    owner: u32,
}

#[derive(Clone, Debug)]
struct PollWaiter {
    fh: u64,
    owner: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct HolderSnapshot {
    pub tgid: u32,
    pub comm: String,
    pub cmdline: String,
    pub open_fhs: usize,
    pub held: usize,
    pub acquired_total: u64,
    pub released_total: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusSnapshot {
    pub ok: bool,
    pub pid: u32,
    pub uptime_secs: u64,
    pub mount_dir: String,
    pub token_path: Option<String>,
    pub capacity: usize,
    pub withheld: usize,
    pub effective_capacity: usize,
    pub held: usize,
    pub available: usize,
    pub max_per_owner: usize,
    pub owners: usize,
    pub open_fhs: usize,
    pub waiters: usize,
    pub pollers: usize,
    pub reclaimed_total: u64,
    pub interrupts_total: u64,
    pub poll_notifies_total: u64,
    pub excess_writes_total: u64,
    pub psi: PsiSnapshot,
    pub holders: Vec<HolderSnapshot>,
}

pub struct JobserverState {
    started: Instant,
    mount_dir: String,
    token_path: Option<String>,
    capacity: usize,
    max_per_owner: usize,
    total_held: usize,
    next_fh: u64,
    fh_to_owner: HashMap<u64, u32>,
    owners: HashMap<u32, OwnerState>,
    read_waiters: VecDeque<ReadWaiter>,
    poll_waiters: HashMap<u64, PollWaiter>,
    reclaimed_total: u64,
    interrupts_total: u64,
    poll_notifies_total: u64,
    excess_writes_total: u64,
    psi: PsiGovernor,
}

impl JobserverState {
    pub fn new(
        capacity: usize,
        max_per_owner: usize,
        psi: PsiGovernor,
        mount_dir: String,
        token_path: Option<String>,
    ) -> Self {
        Self {
            started: Instant::now(),
            mount_dir,
            token_path,
            capacity,
            max_per_owner,
            total_held: 0,
            next_fh: 1,
            fh_to_owner: HashMap::new(),
            owners: HashMap::new(),
            read_waiters: VecDeque::new(),
            poll_waiters: HashMap::new(),
            reclaimed_total: 0,
            interrupts_total: 0,
            poll_notifies_total: 0,
            excess_writes_total: 0,
            psi,
        }
    }

    pub fn effective_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.psi.withheld())
    }

    pub fn available(&self) -> usize {
        self.effective_capacity().saturating_sub(self.total_held)
    }

    pub fn owner_can_acquire(&self, owner: u32) -> bool {
        if self.available() == 0 {
            return false;
        }
        if self.max_per_owner == 0 {
            return true;
        }
        self.owners
            .get(&owner)
            .is_some_and(|st| st.held < self.max_per_owner)
    }

    pub fn on_open(&mut self, owner: u32, comm: String, cmdline: String) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.fh_to_owner.insert(fh, owner);
        self.owners
            .entry(owner)
            .and_modify(|st| {
                st.open_fhs += 1;
                if st.comm.is_empty() && !comm.is_empty() {
                    st.comm = comm.clone();
                }
                if st.cmdline.is_empty() && !cmdline.is_empty() {
                    st.cmdline = cmdline.clone();
                }
            })
            .or_insert(OwnerState {
                comm,
                cmdline,
                open_fhs: 1,
                held: 0,
                acquired_total: 0,
                released_total: 0,
            });
        fh
    }

    fn grant_one(&mut self, owner: u32) -> bool {
        if !self.owner_can_acquire(owner) {
            return false;
        }
        let Some(st) = self.owners.get_mut(&owner) else {
            return false;
        };
        st.held += 1;
        st.acquired_total += 1;
        self.total_held += 1;
        true
    }

    pub fn on_read(&mut self, unique: u64, fh: u64, size: u32, nonblock: bool) -> Option<Effect> {
        if size == 0 {
            return Some(Effect::ReplyError { unique, errno: 0 });
        }
        let Some(&owner) = self.fh_to_owner.get(&fh) else {
            return Some(Effect::ReplyError {
                unique,
                errno: -libc::EBADF,
            });
        };
        if self.grant_one(owner) {
            return Some(Effect::ReplyByte { unique });
        }
        if nonblock {
            return Some(Effect::ReplyError {
                unique,
                errno: -libc::EAGAIN,
            });
        }
        self.read_waiters
            .push_back(ReadWaiter { unique, fh, owner });
        None
    }

    pub fn on_write(&mut self, fh: u64, count: usize) -> (usize, Vec<Effect>) {
        if let Some(&owner) = self.fh_to_owner.get(&fh)
            && let Some(st) = self.owners.get_mut(&owner)
        {
            let returned = count.min(st.held);
            let excess = count.saturating_sub(returned);
            if excess > 0 {
                self.excess_writes_total += excess as u64;
            }
            st.held -= returned;
            st.released_total += returned as u64;
            self.total_held -= returned;
        }
        (count, self.wake_eligible())
    }

    pub fn on_poll(&mut self, fh: u64, kh: u64, schedule_notify: bool, events: u32) -> u32 {
        let mut revents: u32 = libc::POLLOUT as u32;
        if let Some(&owner) = self.fh_to_owner.get(&fh) {
            if self.owner_can_acquire(owner) {
                revents |= libc::POLLIN as u32;
            } else if schedule_notify {
                self.poll_waiters.insert(kh, PollWaiter { fh, owner });
            }
        }
        if events != 0 {
            revents &= events;
        }
        revents
    }

    pub fn on_interrupt(&mut self, target_unique: u64) -> Option<Effect> {
        let pos = self
            .read_waiters
            .iter()
            .position(|w| w.unique == target_unique)?;
        self.read_waiters.remove(pos);
        self.interrupts_total += 1;
        Some(Effect::ReplyError {
            unique: target_unique,
            errno: -libc::EINTR,
        })
    }

    /// Returns `(reclaimed_count, owner_tgid, owner_comm, effects)`.
    pub fn on_release(&mut self, fh: u64) -> (usize, Option<(u32, String)>, Vec<Effect>) {
        let mut effects = Vec::new();
        // Cancel any pending reads/polls tied to this closed file handle.
        let mut idx = 0;
        while idx < self.read_waiters.len() {
            if self.read_waiters[idx].fh == fh {
                let w = self.read_waiters.remove(idx).unwrap();
                effects.push(Effect::ReplyError {
                    unique: w.unique,
                    errno: -libc::EINTR,
                });
            } else {
                idx += 1;
            }
        }
        self.poll_waiters.retain(|_, pw| pw.fh != fh);

        let mut reclaimed = 0usize;
        let mut info = None;
        if let Some(owner) = self.fh_to_owner.remove(&fh) {
            let mut remove_owner = false;
            if let Some(st) = self.owners.get_mut(&owner) {
                st.open_fhs = st.open_fhs.saturating_sub(1);
                if st.open_fhs == 0 {
                    reclaimed = st.held;
                    st.held = 0;
                    remove_owner = true;
                }
            }
            if remove_owner
                && let Some(removed) = self.owners.remove(&owner)
                && reclaimed > 0
            {
                self.total_held -= reclaimed;
                self.reclaimed_total += reclaimed as u64;
                info = Some((owner, removed.comm));
            }
        }
        effects.extend(self.wake_eligible());
        (reclaimed, info, effects)
    }

    pub fn resize(&mut self, new_capacity: usize) -> Result<Vec<Effect>, String> {
        if !(1..=4096).contains(&new_capacity) {
            return Err("capacity must be between 1 and 4096".into());
        }
        self.capacity = new_capacity;
        self.psi.clamp_to_capacity(self.capacity);
        Ok(self.wake_eligible())
    }

    pub fn set_max_per_owner(&mut self, new_cap: usize) -> Result<Vec<Effect>, String> {
        if new_cap > 4096 {
            return Err("max_per_owner must be between 0 (unlimited) and 4096".into());
        }
        self.max_per_owner = new_cap;
        Ok(self.wake_eligible())
    }

    pub fn tick_psi(&mut self) -> Result<(PsiAction, Vec<Effect>), String> {
        let action = self.psi.sample_from_disk(self.capacity)?;
        let effects = match action {
            PsiAction::Released { .. } => self.wake_eligible(),
            _ => Vec::new(),
        };
        Ok((action, effects))
    }

    #[cfg(test)]
    pub fn record_psi_sample(
        &mut self,
        io_full_avg10: f64,
        memory_some_avg10: Option<f64>,
    ) -> (PsiAction, Vec<Effect>) {
        let action = self
            .psi
            .record_sample(io_full_avg10, memory_some_avg10, self.capacity);
        let effects = match action {
            PsiAction::Released { .. } => self.wake_eligible(),
            _ => Vec::new(),
        };
        (action, effects)
    }

    pub fn status(&self) -> StatusSnapshot {
        let mut holders: Vec<HolderSnapshot> = self
            .owners
            .iter()
            .map(|(&tgid, st)| HolderSnapshot {
                tgid,
                comm: st.comm.clone(),
                cmdline: st.cmdline.clone(),
                open_fhs: st.open_fhs,
                held: st.held,
                acquired_total: st.acquired_total,
                released_total: st.released_total,
            })
            .collect();
        holders.sort_by(|a, b| b.held.cmp(&a.held).then_with(|| a.tgid.cmp(&b.tgid)));

        StatusSnapshot {
            ok: true,
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            mount_dir: self.mount_dir.clone(),
            token_path: self.token_path.clone(),
            capacity: self.capacity,
            withheld: self.psi.withheld(),
            effective_capacity: self.effective_capacity(),
            held: self.total_held,
            available: self.available(),
            max_per_owner: self.max_per_owner,
            owners: self.owners.len(),
            open_fhs: self.fh_to_owner.len(),
            waiters: self.read_waiters.len(),
            pollers: self.poll_waiters.len(),
            reclaimed_total: self.reclaimed_total,
            interrupts_total: self.interrupts_total,
            poll_notifies_total: self.poll_notifies_total,
            excess_writes_total: self.excess_writes_total,
            psi: self.psi.snapshot(self.capacity),
            holders,
        }
    }

    fn wake_eligible(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        // Walk blocking readers in FIFO order, skipping capped owners so they
        // cannot head-of-line block uncapped owners behind them.
        let mut idx = 0;
        while self.available() > 0 && idx < self.read_waiters.len() {
            let owner = self.read_waiters[idx].owner;
            if self.owner_can_acquire(owner) {
                let w = self.read_waiters.remove(idx).unwrap();
                let granted = self.grant_one(w.owner);
                debug_assert!(granted);
                effects.push(Effect::ReplyByte { unique: w.unique });
            } else {
                idx += 1;
            }
        }
        // Only notify pollers whose owner is actually eligible to acquire right
        // now, preventing busy-spin loops on per-owner-capped clients.
        if self.available() > 0 && !self.poll_waiters.is_empty() {
            let eligible_khs: Vec<u64> = self
                .poll_waiters
                .iter()
                .filter(|&(_, pw)| self.owner_can_acquire(pw.owner))
                .map(|(&kh, _)| kh)
                .collect();
            for kh in eligible_khs {
                self.poll_waiters.remove(&kh);
                self.poll_notifies_total += 1;
                effects.push(Effect::NotifyPoll { kh });
            }
        }
        effects
    }
}

pub fn resolve_owner_metadata(tid: u32) -> (u32, String, String) {
    let mut tgid = tid;
    if let Ok(status) = fs::read_to_string(format!("/proc/{tid}/status")) {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Tgid:")
                && let Ok(parsed) = rest.trim().parse::<u32>()
            {
                tgid = parsed;
                break;
            }
        }
    }
    let comm = fs::read_to_string(format!("/proc/{tgid}/comm"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let cmdline = fs::read(format!("/proc/{tgid}/cmdline"))
        .map(|raw| {
            let mut s = raw
                .split(|&b| b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if s.len() > 240 {
                s.truncate(240);
            }
            s
        })
        .unwrap_or_default();
    (tgid, comm, cmdline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psi::PsiConfig;

    fn make_state(capacity: usize, max_per_owner: usize) -> JobserverState {
        let cfg = PsiConfig {
            enabled: true,
            ..Default::default()
        };
        JobserverState::new(
            capacity,
            max_per_owner,
            PsiGovernor::new(cfg),
            "/mnt".into(),
            None,
        )
    }

    #[test]
    fn dual_open_accounting_and_reclaim_on_last_release() {
        let mut st = make_state(4, 0);
        let fh_r = st.on_open(100, "cargo".into(), "cargo build".into());
        let fh_w = st.on_open(100, "cargo".into(), "cargo build".into());
        assert_eq!(
            st.on_read(1, fh_r, 1, false),
            Some(Effect::ReplyByte { unique: 1 })
        );
        assert_eq!(
            st.on_read(2, fh_r, 1, false),
            Some(Effect::ReplyByte { unique: 2 })
        );
        assert_eq!(
            st.on_read(3, fh_r, 1, false),
            Some(Effect::ReplyByte { unique: 3 })
        );
        assert_eq!(st.available(), 1);

        // Return 1 via separate write fh -> matches same TGID owner.
        let (n, eff) = st.on_write(fh_w, 1);
        assert_eq!(n, 1);
        assert!(eff.is_empty());
        assert_eq!(st.available(), 2);

        // First close (fh_r) does NOT reclaim yet while fh_w is still open.
        let (reclaimed, _, _) = st.on_release(fh_r);
        assert_eq!(reclaimed, 0);
        assert_eq!(st.available(), 2);

        // Last close (fh_w) reclaims remaining 2 held tokens immediately.
        let (reclaimed, info, _) = st.on_release(fh_w);
        assert_eq!(reclaimed, 2);
        assert_eq!(info, Some((100, "cargo".into())));
        assert_eq!(st.available(), 4);
        assert_eq!(st.status().held, 0);
        assert_eq!(st.status().reclaimed_total, 2);
    }

    #[test]
    fn per_owner_cap_prevents_hol_blocking_and_poll_spin() {
        let mut st = make_state(4, 2);
        let a = st.on_open(10, "rustc".into(), "rustc a".into());
        let b = st.on_open(20, "rustc".into(), "rustc b".into());

        // Owner A takes 2 (hits cap=2), even though 2 tokens remain globally available.
        assert_eq!(
            st.on_read(1, a, 1, false),
            Some(Effect::ReplyByte { unique: 1 })
        );
        assert_eq!(
            st.on_read(2, a, 1, false),
            Some(Effect::ReplyByte { unique: 2 })
        );
        assert_eq!(st.available(), 2);

        // A's 3rd nonblocking read gets EAGAIN; poll does NOT report POLLIN.
        assert_eq!(
            st.on_read(3, a, 1, true),
            Some(Effect::ReplyError {
                unique: 3,
                errno: -libc::EAGAIN
            })
        );
        let rev = st.on_poll(a, 777, true, libc::POLLIN as u32);
        assert_eq!(rev, 0);

        // A parks a blocking read; B still gets both remaining tokens (no HOL block).
        assert_eq!(st.on_read(4, a, 1, false), None);
        assert_eq!(
            st.on_read(5, b, 1, false),
            Some(Effect::ReplyByte { unique: 5 })
        );
        assert_eq!(
            st.on_read(6, b, 1, false),
            Some(Effect::ReplyByte { unique: 6 })
        );
        assert_eq!(st.available(), 0);

        // B also parks behind A. When B returns 1, B (not capped A) gets woken!
        assert_eq!(st.on_read(7, b, 1, false), None);
        let (_, eff) = st.on_write(b, 1);
        assert_eq!(eff, vec![Effect::ReplyByte { unique: 7 }]);

        // When A returns 1, A drops below cap=2 and wakes its own parked read + poll.
        let (_, eff) = st.on_write(a, 1);
        assert_eq!(eff, vec![Effect::ReplyByte { unique: 4 }]);
        // Raising cap allows A's poller to wake once a token frees.
        let _ = st.on_write(b, 1);
        let eff = st.set_max_per_owner(4).unwrap();
        assert_eq!(eff, vec![Effect::NotifyPoll { kh: 777 }]);
    }

    #[test]
    fn excess_write_cannot_inflate_pool() {
        let mut st = make_state(2, 0);
        let fh = st.on_open(42, "rogue".into(), String::new());
        let (n, _) = st.on_write(fh, 5);
        assert_eq!(n, 5);
        assert_eq!(st.available(), 2);
        assert_eq!(st.status().excess_writes_total, 5);
    }

    #[test]
    fn psi_withholding_and_release_wakes_waiters() {
        let mut st = make_state(8, 0);
        let fh = st.on_open(55, "cargo".into(), String::new());
        // Take 4 tokens (4 remain available).
        for u in 1..=4 {
            assert_eq!(
                st.on_read(u, fh, 1, false),
                Some(Effect::ReplyByte { unique: u })
            );
        }
        // 3 high PSI samples withhold 4 tokens -> effective_capacity=4 -> available=0.
        st.record_psi_sample(30.0, Some(1.0));
        st.record_psi_sample(30.0, Some(1.0));
        let (act, _) = st.record_psi_sample(30.0, Some(1.0));
        assert_eq!(
            act,
            PsiAction::Withheld {
                delta: 4,
                new_withheld: 4
            }
        );
        assert_eq!(st.available(), 0);

        // Next read blocks until PSI recovers.
        assert_eq!(st.on_read(99, fh, 1, false), None);
        // [30, 30, 1] -> avg 20.33 > 20 -> withholds 2 more up to cap=6.
        assert_eq!(
            st.record_psi_sample(1.0, Some(0.0)).0,
            PsiAction::Withheld {
                delta: 2,
                new_withheld: 6
            }
        );
        // [30, 1, 1] -> avg 10.67 (in hysteresis band [8, 20]) -> unchanged.
        assert_eq!(st.record_psi_sample(1.0, Some(0.0)).0, PsiAction::Unchanged);
        // [1, 1, 1] -> avg 1.0 < 8 -> releases 4 (6 -> 2), effective=6, wakes waiter 99!
        let (act, eff) = st.record_psi_sample(1.0, Some(0.0));
        assert_eq!(
            act,
            PsiAction::Released {
                delta: 4,
                new_withheld: 2
            }
        );
        assert_eq!(eff, vec![Effect::ReplyByte { unique: 99 }]);
        assert_eq!(st.available(), 1);
        // Next low sample releases final 2 (2 -> 0), effective=8, available=3.
        assert_eq!(
            st.record_psi_sample(1.0, Some(0.0)).0,
            PsiAction::Released {
                delta: 2,
                new_withheld: 0
            }
        );
        assert_eq!(st.available(), 3);
    }

    #[test]
    fn interrupt_cancels_parked_read() {
        let mut st = make_state(1, 0);
        let fh = st.on_open(1, "a".into(), String::new());
        assert!(st.on_read(1, fh, 1, false).is_some());
        assert!(st.on_read(2, fh, 1, false).is_none());
        assert_eq!(
            st.on_interrupt(2),
            Some(Effect::ReplyError {
                unique: 2,
                errno: -libc::EINTR
            })
        );
        assert_eq!(st.status().waiters, 0);
        assert_eq!(st.status().interrupts_total, 1);
    }
}
