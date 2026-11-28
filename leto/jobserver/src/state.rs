use std::collections::{HashMap, VecDeque};
use std::fs;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::psi::{PsiAction, PsiGovernor, PsiSnapshot};

/// How long a token stays reserved for a notified poll client before returning
/// to the pool if the client fails to follow up with `read(1)`.
pub const POLL_RESERVATION_TTL: Duration = Duration::from_millis(50);

const MAX_STATUS_HOLDERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OwnerKey {
    /// Process identity `(tgid, starttime_ticks)` to prevent PID reuse from
    /// merging distinct processes.
    Process { tgid: u32, starttime: u64 },
    /// Synthetic per-handle owner when `hdr.pid == 0` (caller in another PID
    /// namespace).
    Synthetic(u64),
}

impl OwnerKey {
    pub fn tgid(self) -> u32 {
        match self {
            Self::Process { tgid, .. } => tgid,
            Self::Synthetic(_) => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    ReplyByte { unique: u64, owner: OwnerKey },
    ReplyError { unique: u64, errno: i32 },
    NotifyPoll { kh: u64 },
}

#[derive(Clone, Debug)]
struct OwnerState {
    comm: String,
    open_fhs: usize,
    held: usize,
    acquired_total: u64,
    released_total: u64,
}

#[derive(Clone, Debug)]
enum WaitEntry {
    Read {
        unique: u64,
        fh: u64,
        owner: OwnerKey,
    },
    Poll {
        kh: u64,
        fh: u64,
        owner: OwnerKey,
    },
}

impl WaitEntry {
    fn fh(&self) -> u64 {
        match *self {
            Self::Read { fh, .. } | Self::Poll { fh, .. } => fh,
        }
    }

    fn owner(&self) -> OwnerKey {
        match *self {
            Self::Read { owner, .. } | Self::Poll { owner, .. } => owner,
        }
    }
}

#[derive(Clone, Debug)]
struct PollReservation {
    fh: u64,
    owner: OwnerKey,
    expires: Instant,
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
    pub draining: bool,
    pub mount_dir: String,
    pub token_path: Option<String>,
    pub capacity: usize,
    pub withheld: usize,
    pub effective_capacity: usize,
    pub held: usize,
    pub reserved: usize,
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
    pub holders_truncated: bool,
    pub psi: PsiSnapshot,
    pub holders: Vec<HolderSnapshot>,
}

pub struct JobserverState {
    started: Instant,
    draining: bool,
    mount_dir: String,
    token_path: Option<String>,
    capacity: usize,
    max_per_owner: usize,
    total_held: usize,
    next_fh: u64,
    fh_to_owner: HashMap<u64, OwnerKey>,
    owners: HashMap<OwnerKey, OwnerState>,
    // Unified FIFO arrival queue of blocking readers and pollers prevents poll
    // clients (cc-rs, GNU make) from starving behind blocking rustc helpers.
    wait_queue: VecDeque<WaitEntry>,
    reservations: VecDeque<PollReservation>,
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
            draining: false,
            mount_dir,
            token_path,
            capacity,
            max_per_owner,
            total_held: 0,
            next_fh: 1,
            fh_to_owner: HashMap::new(),
            owners: HashMap::new(),
            wait_queue: VecDeque::new(),
            reservations: VecDeque::new(),
            reclaimed_total: 0,
            interrupts_total: 0,
            poll_notifies_total: 0,
            excess_writes_total: 0,
            psi,
        }
    }

    pub fn set_draining(&mut self) {
        self.draining = true;
    }

    pub fn open_fhs(&self) -> usize {
        self.fh_to_owner.len()
    }

    pub fn has_reservations(&self) -> bool {
        !self.reservations.is_empty()
    }

    pub fn effective_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.psi.withheld())
    }

    pub fn available(&self) -> usize {
        self.effective_capacity()
            .saturating_sub(self.total_held + self.reservations.len())
    }

    fn owner_reserved(&self, owner: OwnerKey) -> usize {
        self.reservations
            .iter()
            .filter(|r| r.owner == owner)
            .count()
    }

    fn owner_under_cap(&self, owner: OwnerKey) -> bool {
        if self.max_per_owner == 0 {
            return true;
        }
        let reserved = self.owner_reserved(owner);
        self.owners
            .get(&owner)
            .is_some_and(|st| st.held + reserved < self.max_per_owner)
    }

    pub fn owner_can_acquire(&self, owner: OwnerKey) -> bool {
        self.available() > 0 && self.owner_under_cap(owner)
    }

    fn fh_has_reservation(&self, fh: u64) -> bool {
        self.reservations.iter().any(|r| r.fh == fh)
    }

    pub fn on_open(&mut self, pid_info: Option<(u32, u64)>, comm: String) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        let owner = match pid_info {
            Some((tgid, starttime)) => OwnerKey::Process { tgid, starttime },
            None => OwnerKey::Synthetic(fh),
        };
        self.fh_to_owner.insert(fh, owner);
        self.owners
            .entry(owner)
            .and_modify(|st| {
                st.open_fhs += 1;
                if st.comm.is_empty() && !comm.is_empty() {
                    st.comm = comm.clone();
                }
            })
            .or_insert(OwnerState {
                comm,
                open_fhs: 1,
                held: 0,
                acquired_total: 0,
                released_total: 0,
            });
        fh
    }

    fn grant_one(&mut self, owner: OwnerKey) -> bool {
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

    fn consume_reservation(&mut self, fh: u64, owner: OwnerKey) -> bool {
        let Some(idx) = self.reservations.iter().position(|r| r.fh == fh) else {
            return false;
        };
        self.reservations.remove(idx);
        if let Some(st) = self.owners.get_mut(&owner) {
            st.held += 1;
            st.acquired_total += 1;
            self.total_held += 1;
            true
        } else {
            false
        }
    }

    /// Reverses a grant when writing the FUSE reply to `/dev/fuse` fails
    /// (for example `ENOENT` if the kernel aborted the request mid-reply).
    pub fn rollback_grant(&mut self, owner: OwnerKey, now: Instant) -> Vec<Effect> {
        if let Some(st) = self.owners.get_mut(&owner)
            && st.held > 0
        {
            st.held -= 1;
            self.total_held -= 1;
        }
        self.wake_eligible(now)
    }

    pub fn expire_reservations(&mut self, now: Instant) -> Vec<Effect> {
        let before = self.reservations.len();
        self.reservations.retain(|r| r.expires > now);
        if self.reservations.len() < before {
            self.wake_eligible(now)
        } else {
            Vec::new()
        }
    }

    pub fn on_read(
        &mut self,
        unique: u64,
        fh: u64,
        size: u32,
        nonblock: bool,
        now: Instant,
    ) -> Vec<Effect> {
        if size == 0 {
            return vec![Effect::ReplyError { unique, errno: 0 }];
        }
        let Some(&owner) = self.fh_to_owner.get(&fh) else {
            return vec![Effect::ReplyError {
                unique,
                errno: -libc::EBADF,
            }];
        };
        if self.consume_reservation(fh, owner) {
            return vec![Effect::ReplyByte { unique, owner }];
        }
        let mut effects = self.expire_reservations(now);
        // Only bypass the queue when no eligible waiter is already ahead, or
        // when this owner has no waiting peers ahead of it.
        let eligible_waiting = self
            .wait_queue
            .iter()
            .any(|w| self.owner_under_cap(w.owner()));
        if !eligible_waiting && self.grant_one(owner) {
            effects.push(Effect::ReplyByte { unique, owner });
            return effects;
        }
        if nonblock {
            effects.push(Effect::ReplyError {
                unique,
                errno: -libc::EAGAIN,
            });
            return effects;
        }
        self.wait_queue
            .push_back(WaitEntry::Read { unique, fh, owner });
        effects.extend(self.wake_eligible(now));
        effects
    }

    /// Returns `(bytes_accepted, excess_count, effects)`.
    pub fn on_write(&mut self, fh: u64, count: usize, now: Instant) -> (usize, usize, Vec<Effect>) {
        let mut excess = 0usize;
        if let Some(&owner) = self.fh_to_owner.get(&fh)
            && let Some(st) = self.owners.get_mut(&owner)
        {
            let returned = count.min(st.held);
            excess = count.saturating_sub(returned);
            if excess > 0 {
                self.excess_writes_total += excess as u64;
            }
            st.held -= returned;
            st.released_total += returned as u64;
            self.total_held -= returned;
        }
        (count, excess, self.wake_eligible(now))
    }

    pub fn on_poll(
        &mut self,
        fh: u64,
        kh: u64,
        schedule_notify: bool,
        events: u32,
        now: Instant,
    ) -> u32 {
        self.reservations.retain(|r| r.expires > now);
        let mut revents: u32 = libc::POLLOUT as u32;
        if let Some(&owner) = self.fh_to_owner.get(&fh) {
            let eligible_waiting = self
                .wait_queue
                .iter()
                .any(|w| self.owner_under_cap(w.owner()));
            if self.fh_has_reservation(fh) || (!eligible_waiting && self.owner_can_acquire(owner)) {
                revents |= libc::POLLIN as u32;
            } else if schedule_notify {
                // Keep existing queue position if this kh is already queued.
                let already = self
                    .wait_queue
                    .iter()
                    .any(|w| matches!(w, WaitEntry::Poll { kh: existing, .. } if *existing == kh));
                if !already {
                    self.wait_queue.push_back(WaitEntry::Poll { kh, fh, owner });
                }
            }
        }
        if events != 0 {
            revents &= events;
        }
        revents
    }

    pub fn on_interrupt(&mut self, target_unique: u64) -> Option<Effect> {
        let pos = self.wait_queue.iter().position(
            |w| matches!(w, WaitEntry::Read { unique, .. } if *unique == target_unique),
        )?;
        self.wait_queue.remove(pos);
        self.interrupts_total += 1;
        Some(Effect::ReplyError {
            unique: target_unique,
            errno: -libc::EINTR,
        })
    }

    /// Returns `(reclaimed_count, owner_info, effects)`.
    pub fn on_release(
        &mut self,
        fh: u64,
        now: Instant,
    ) -> (usize, Option<(u32, String)>, Vec<Effect>) {
        let mut effects = Vec::new();
        let mut idx = 0;
        while idx < self.wait_queue.len() {
            if self.wait_queue[idx].fh() == fh {
                if let Some(WaitEntry::Read { unique, .. }) = self.wait_queue.remove(idx) {
                    effects.push(Effect::ReplyError {
                        unique,
                        errno: -libc::EINTR,
                    });
                }
            } else {
                idx += 1;
            }
        }
        self.reservations.retain(|r| r.fh != fh);

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
                info = Some((owner.tgid(), removed.comm));
            }
        }
        effects.extend(self.wake_eligible(now));
        (reclaimed, info, effects)
    }

    pub fn resize(&mut self, new_capacity: usize, now: Instant) -> Result<Vec<Effect>, String> {
        if !(1..=4096).contains(&new_capacity) {
            return Err("capacity must be between 1 and 4096".into());
        }
        self.capacity = new_capacity;
        self.psi.clamp_to_capacity(self.capacity);
        Ok(self.wake_eligible(now))
    }

    pub fn set_max_per_owner(
        &mut self,
        new_cap: usize,
        now: Instant,
    ) -> Result<Vec<Effect>, String> {
        if new_cap > 4096 {
            return Err("max_per_owner must be between 0 (unlimited) and 4096".into());
        }
        self.max_per_owner = new_cap;
        Ok(self.wake_eligible(now))
    }

    pub fn tick_psi(&mut self, now: Instant) -> (PsiAction, Option<String>, Vec<Effect>) {
        let (action, err) = self.psi.sample_from_disk(self.capacity);
        let effects = match action {
            PsiAction::Released { .. } => self.wake_eligible(now),
            _ => self.expire_reservations(now),
        };
        (action, err, effects)
    }

    #[cfg(test)]
    pub fn record_psi_sample(
        &mut self,
        io_full_avg10: f64,
        memory_some_avg10: Option<f64>,
        now: Instant,
    ) -> (PsiAction, Vec<Effect>) {
        let action = self
            .psi
            .record_sample(io_full_avg10, memory_some_avg10, self.capacity);
        let effects = match action {
            PsiAction::Released { .. } => self.wake_eligible(now),
            _ => Vec::new(),
        };
        (action, effects)
    }

    pub fn status(&self) -> StatusSnapshot {
        let mut entries: Vec<(OwnerKey, &OwnerState)> =
            self.owners.iter().map(|(&k, v)| (k, v)).collect();
        entries.sort_by(|a, b| {
            b.1.held
                .cmp(&a.1.held)
                .then_with(|| a.0.tgid().cmp(&b.0.tgid()))
        });
        let holders_truncated = entries.len() > MAX_STATUS_HOLDERS;
        let holders: Vec<HolderSnapshot> = entries
            .into_iter()
            .take(MAX_STATUS_HOLDERS)
            .map(|(key, st)| {
                let tgid = key.tgid();
                HolderSnapshot {
                    tgid,
                    comm: st.comm.clone(),
                    cmdline: read_cmdline_lazy(tgid),
                    open_fhs: st.open_fhs,
                    held: st.held,
                    acquired_total: st.acquired_total,
                    released_total: st.released_total,
                }
            })
            .collect();

        let waiters = self
            .wait_queue
            .iter()
            .filter(|w| matches!(w, WaitEntry::Read { .. }))
            .count();
        let pollers = self
            .wait_queue
            .iter()
            .filter(|w| matches!(w, WaitEntry::Poll { .. }))
            .count();

        StatusSnapshot {
            ok: true,
            pid: std::process::id(),
            uptime_secs: self.started.elapsed().as_secs(),
            draining: self.draining,
            mount_dir: self.mount_dir.clone(),
            token_path: self.token_path.clone(),
            capacity: self.capacity,
            withheld: self.psi.withheld(),
            effective_capacity: self.effective_capacity(),
            held: self.total_held,
            reserved: self.reservations.len(),
            available: self.available(),
            max_per_owner: self.max_per_owner,
            owners: self.owners.len(),
            open_fhs: self.fh_to_owner.len(),
            waiters,
            pollers,
            reclaimed_total: self.reclaimed_total,
            interrupts_total: self.interrupts_total,
            poll_notifies_total: self.poll_notifies_total,
            excess_writes_total: self.excess_writes_total,
            holders_truncated,
            psi: self.psi.snapshot(self.capacity),
            holders,
        }
    }

    fn wake_eligible(&mut self, now: Instant) -> Vec<Effect> {
        self.reservations.retain(|r| r.expires > now);
        let mut effects = Vec::new();
        let mut idx = 0;
        while self.available() > 0 && idx < self.wait_queue.len() {
            let owner = self.wait_queue[idx].owner();
            if !self.owner_can_acquire(owner) {
                idx += 1;
                continue;
            }
            match self.wait_queue.remove(idx).unwrap() {
                WaitEntry::Read { unique, owner, .. } => {
                    let granted = self.grant_one(owner);
                    debug_assert!(granted);
                    effects.push(Effect::ReplyByte { unique, owner });
                }
                WaitEntry::Poll { kh, fh, owner } => {
                    self.reservations.push_back(PollReservation {
                        fh,
                        owner,
                        expires: now + POLL_RESERVATION_TTL,
                    });
                    self.poll_notifies_total += 1;
                    effects.push(Effect::NotifyPoll { kh });
                }
            }
        }
        effects
    }
}

/// Reads `/proc/<tid>/status` (Tgid + Name) and `/proc/<tgid>/stat` (starttime)
/// without touching `/proc/<tgid>/cmdline` (which takes the target's `mmap_lock`).
/// Returns `(None, "")` when `tid == 0` (caller in another PID namespace).
pub fn resolve_open_identity(tid: u32) -> (Option<(u32, u64)>, String) {
    if tid == 0 {
        return (None, String::new());
    }
    let mut tgid = tid;
    let mut comm = String::new();
    if let Ok(status) = fs::read_to_string(format!("/proc/{tid}/status")) {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Name:") {
                comm = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("Tgid:")
                && let Ok(parsed) = rest.trim().parse::<u32>()
            {
                tgid = parsed;
            }
        }
    }
    let starttime = read_proc_starttime(tgid).unwrap_or(0);
    (Some((tgid, starttime)), comm)
}

fn read_proc_starttime(tgid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{tgid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Field 22 (1-based) is index 19 (0-based) after the closing `)` of comm.
    after_comm.split_whitespace().nth(19)?.parse::<u64>().ok()
}

fn read_cmdline_lazy(tgid: u32) -> String {
    if tgid == 0 {
        return String::new();
    }
    fs::read(format!("/proc/{tgid}/cmdline"))
        .map(|raw| {
            let mut s = raw
                .split(|&b| b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if s.len() > 240 {
                let mut end = 240;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
            }
            s
        })
        .unwrap_or_default()
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

    fn proc_owner(tgid: u32) -> OwnerKey {
        OwnerKey::Process { tgid, starttime: 1 }
    }

    #[test]
    fn dual_open_accounting_and_reclaim_on_last_release() {
        let now = Instant::now();
        let mut st = make_state(4, 0);
        let fh_r = st.on_open(Some((100, 1)), "cargo".into());
        let fh_w = st.on_open(Some((100, 1)), "cargo".into());
        assert_eq!(
            st.on_read(1, fh_r, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 1,
                owner: proc_owner(100)
            }]
        );
        assert_eq!(
            st.on_read(2, fh_r, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 2,
                owner: proc_owner(100)
            }]
        );
        assert_eq!(
            st.on_read(3, fh_r, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 3,
                owner: proc_owner(100)
            }]
        );
        assert_eq!(st.available(), 1);

        let (n, excess, eff) = st.on_write(fh_w, 1, now);
        assert_eq!((n, excess), (1, 0));
        assert!(eff.is_empty());
        assert_eq!(st.available(), 2);

        let (reclaimed, _, _) = st.on_release(fh_r, now);
        assert_eq!(reclaimed, 0);
        assert_eq!(st.available(), 2);

        let (reclaimed, info, _) = st.on_release(fh_w, now);
        assert_eq!(reclaimed, 2);
        assert_eq!(info, Some((100, "cargo".into())));
        assert_eq!(st.available(), 4);
        assert_eq!(st.status().held, 0);
        assert_eq!(st.status().reclaimed_total, 2);
    }

    #[test]
    fn pid_zero_creates_distinct_synthetic_owners() {
        let now = Instant::now();
        let mut st = make_state(4, 1);
        let fh1 = st.on_open(None, String::new());
        let fh2 = st.on_open(None, String::new());
        assert_eq!(st.on_read(1, fh1, 1, true, now).len(), 1);
        // fh2 is a separate synthetic owner so cap=1 on fh1 does not block fh2.
        assert_eq!(
            st.on_read(2, fh2, 1, true, now),
            vec![Effect::ReplyByte {
                unique: 2,
                owner: OwnerKey::Synthetic(fh2)
            }]
        );
        let (reclaimed, _, _) = st.on_release(fh1, now);
        assert_eq!(reclaimed, 1);
        assert_eq!(st.available(), 3);
    }

    #[test]
    fn per_owner_cap_prevents_hol_blocking_and_poll_spin() {
        let now = Instant::now();
        let mut st = make_state(4, 2);
        let a = st.on_open(Some((10, 1)), "rustc".into());
        let b = st.on_open(Some((20, 1)), "rustc".into());

        assert_eq!(
            st.on_read(1, a, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 1,
                owner: proc_owner(10)
            }]
        );
        assert_eq!(
            st.on_read(2, a, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 2,
                owner: proc_owner(10)
            }]
        );
        assert_eq!(st.available(), 2);

        assert_eq!(
            st.on_read(3, a, 1, true, now),
            vec![Effect::ReplyError {
                unique: 3,
                errno: -libc::EAGAIN
            }]
        );
        let rev = st.on_poll(a, 777, true, libc::POLLIN as u32, now);
        assert_eq!(rev, 0);

        assert!(st.on_read(4, a, 1, false, now).is_empty());
        assert_eq!(
            st.on_read(5, b, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 5,
                owner: proc_owner(20)
            }]
        );
        assert_eq!(
            st.on_read(6, b, 1, false, now),
            vec![Effect::ReplyByte {
                unique: 6,
                owner: proc_owner(20)
            }]
        );
        assert_eq!(st.available(), 0);

        assert!(st.on_read(7, b, 1, false, now).is_empty());
        let (_, _, eff) = st.on_write(b, 1, now);
        assert_eq!(
            eff,
            vec![Effect::ReplyByte {
                unique: 7,
                owner: proc_owner(20)
            }]
        );

        let (_, _, eff) = st.on_write(a, 1, now);
        assert_eq!(eff, vec![Effect::NotifyPoll { kh: 777 }]);
        // Consuming reservation via read(a) grants A its token.
        assert_eq!(
            st.on_read(8, a, 1, true, now),
            vec![Effect::ReplyByte {
                unique: 8,
                owner: proc_owner(10)
            }]
        );
        // Raising cap + freeing one wakes A's parked blocking read (unique 4).
        let _ = st.on_write(b, 1, now);
        let eff = st.set_max_per_owner(4, now).unwrap();
        assert_eq!(
            eff,
            vec![Effect::ReplyByte {
                unique: 4,
                owner: proc_owner(10)
            }]
        );
    }

    #[test]
    fn poll_client_does_not_starve_behind_blocking_readers() {
        let now = Instant::now();
        let mut st = make_state(1, 0);
        let holder = st.on_open(Some((1, 1)), "h".into());
        let poller = st.on_open(Some((2, 1)), "make".into());
        let reader = st.on_open(Some((3, 1)), "rustc".into());

        assert_eq!(st.on_read(1, holder, 1, false, now).len(), 1);
        // Poller registers first, then blocking reader parks behind it.
        assert_eq!(st.on_poll(poller, 555, true, libc::POLLIN as u32, now), 0);
        assert!(st.on_read(2, reader, 1, false, now).is_empty());

        // Returning the token reserves it for the earlier poller, not the later reader!
        let (_, _, eff) = st.on_write(holder, 1, now);
        assert_eq!(eff, vec![Effect::NotifyPoll { kh: 555 }]);
        assert_eq!(st.available(), 0);
        assert_eq!(
            st.on_read(3, poller, 1, true, now),
            vec![Effect::ReplyByte {
                unique: 3,
                owner: proc_owner(2)
            }]
        );

        // If a reservation expires unclaimed, the blocking reader behind it gets woken.
        let (_, _, eff) = st.on_write(poller, 1, now);
        assert_eq!(
            eff,
            vec![Effect::ReplyByte {
                unique: 2,
                owner: proc_owner(3)
            }]
        );
    }

    #[test]
    fn rollback_grant_returns_token_and_wakes_next_waiter() {
        let now = Instant::now();
        let mut st = make_state(1, 0);
        let a = st.on_open(Some((1, 1)), "a".into());
        let b = st.on_open(Some((2, 1)), "b".into());
        assert_eq!(st.on_read(1, a, 1, false, now).len(), 1);
        assert!(st.on_read(2, b, 1, false, now).is_empty());

        let eff = st.rollback_grant(proc_owner(1), now);
        assert_eq!(
            eff,
            vec![Effect::ReplyByte {
                unique: 2,
                owner: proc_owner(2)
            }]
        );
        assert_eq!(st.status().held, 1);
    }

    #[test]
    fn excess_write_cannot_inflate_pool() {
        let now = Instant::now();
        let mut st = make_state(2, 0);
        let fh = st.on_open(Some((42, 1)), "rogue".into());
        let (n, excess, _) = st.on_write(fh, 5, now);
        assert_eq!((n, excess), (5, 5));
        assert_eq!(st.available(), 2);
        assert_eq!(st.status().excess_writes_total, 5);
    }

    #[test]
    fn psi_withholding_and_release_wakes_waiters() {
        let now = Instant::now();
        let mut st = make_state(8, 0);
        let fh = st.on_open(Some((55, 1)), "cargo".into());
        for u in 1..=4 {
            assert_eq!(
                st.on_read(u, fh, 1, false, now),
                vec![Effect::ReplyByte {
                    unique: u,
                    owner: proc_owner(55)
                }]
            );
        }
        st.record_psi_sample(30.0, Some(1.0), now);
        st.record_psi_sample(30.0, Some(1.0), now);
        let (act, _) = st.record_psi_sample(30.0, Some(1.0), now);
        assert_eq!(
            act,
            PsiAction::Withheld {
                delta: 4,
                new_withheld: 4
            }
        );
        assert_eq!(st.available(), 0);

        assert!(st.on_read(99, fh, 1, false, now).is_empty());
        assert_eq!(
            st.record_psi_sample(1.0, Some(0.0), now).0,
            PsiAction::Withheld {
                delta: 2,
                new_withheld: 6
            }
        );
        assert_eq!(
            st.record_psi_sample(1.0, Some(0.0), now).0,
            PsiAction::Unchanged
        );
        let (act, eff) = st.record_psi_sample(1.0, Some(0.0), now);
        assert_eq!(
            act,
            PsiAction::Released {
                delta: 4,
                new_withheld: 2
            }
        );
        assert_eq!(
            eff,
            vec![Effect::ReplyByte {
                unique: 99,
                owner: proc_owner(55)
            }]
        );
        assert_eq!(st.available(), 1);
        assert_eq!(
            st.record_psi_sample(1.0, Some(0.0), now).0,
            PsiAction::Released {
                delta: 2,
                new_withheld: 0
            }
        );
        assert_eq!(st.available(), 3);
    }

    #[test]
    fn interrupt_cancels_parked_read() {
        let now = Instant::now();
        let mut st = make_state(1, 0);
        let fh = st.on_open(Some((1, 1)), "a".into());
        assert_eq!(st.on_read(1, fh, 1, false, now).len(), 1);
        assert!(st.on_read(2, fh, 1, false, now).is_empty());
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
