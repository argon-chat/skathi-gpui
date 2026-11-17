//! vue-native fork: which window presents reached the screen, and when (`gpui::present_feedback`).
//!
//! A flip-model swap chain's frame statistics name the last present shown (`PresentCount`, as
//! `GetLastPresentCount` numbers them) and the refresh it was first shown on
//! (`PresentRefreshCount`), against a refresh and its QPC time (`SyncRefreshCount`,
//! `SyncQPCTime`). The vsync thread reads them every refresh while feedback is wanted. DWM
//! discards a present superseded before its refresh, so a present older than one shown that never
//! showed itself was discarded. A swap chain whose statistics stay empty has its presents estimated
//! instead: shown one refresh after the first vblank following their GPU work.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use gpui::present_feedback::{PresentOutcome, present_feedback_wanted, present_listener};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::ID3D12Fence;
use windows::Win32::Graphics::Dwm::{DWM_TIMING_INFO, DwmGetCompositionTimingInfo};
use windows::Win32::Graphics::Dxgi::{DXGI_FRAME_STATISTICS, IDXGISwapChain};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::Interface;

/// Refreshes a present may wait for statistics before the swap chain is taken to have none.
const PROBE_REFRESHES: i64 = 8;
const KEPT_VBLANKS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Pending {
    present: u64,
    count: u32,
    queued: i64,
    fence: u64,
    /// When its GPU work was known done; a lower bound, from the poll before the one that saw it.
    ready: Option<i64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Stats {
    present_count: u32,
    present_refresh: u32,
    sync_refresh: u32,
    sync_qpc: i64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Poll {
    now: i64,
    period: i64,
    vblank: i64,
    stats: Option<Stats>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Displayed { at: i64, refresh: i64, exact: bool },
    Discarded,
}

#[derive(Default, PartialEq, Eq, Debug)]
enum Mode {
    #[default]
    Probing,
    Exact,
    Estimated,
}

#[derive(Default)]
pub(crate) struct Decoder {
    mode: Mode,
    /// The last present seen shown and its refresh.
    shown: Option<(u32, u32)>,
    /// Refreshes the statistics were sampled on, with their QPC times.
    vblanks: VecDeque<(u32, i64)>,
    last_poll: Option<i64>,
}

fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

fn offset(refresh: u32, from: u32) -> i64 {
    refresh.wrapping_sub(from) as i32 as i64
}

fn ceil_div(a: i64, b: i64) -> i64 {
    -(-a).div_euclid(b)
}

impl Decoder {
    pub(crate) fn decode(
        &mut self,
        pending: &mut VecDeque<Pending>,
        poll: Poll,
        done: impl Fn(u64) -> bool,
    ) -> Vec<(u64, Outcome)> {
        let mut out = Vec::new();
        let period = poll.period.max(1);
        for p in pending.iter_mut().filter(|p| p.ready.is_none()) {
            if done(p.fence) {
                p.ready = Some(p.queued.max(self.last_poll.unwrap_or(p.queued)));
            }
        }
        let cutoff = poll.now - PROBE_REFRESHES * period;
        let overdue = pending.front().is_some_and(|p| p.queued < cutoff);
        let sampled = poll.stats.filter(|s| s.sync_qpc != 0);
        if let Some(s) = sampled {
            if self
                .vblanks
                .back()
                .is_none_or(|(r, _)| *r != s.sync_refresh)
            {
                self.vblanks.push_back((s.sync_refresh, s.sync_qpc));
                if self.vblanks.len() > KEPT_VBLANKS {
                    self.vblanks.pop_front();
                }
            }
        }
        match sampled.filter(|s| s.present_count != 0) {
            Some(stats) => {
                self.mode = Mode::Exact;
                self.exact(pending, stats, period, &mut out);
            }
            None if self.mode == Mode::Probing && overdue => self.mode = Mode::Estimated,
            None => {}
        }
        // Statistics can lag far behind (DWM updates a composed window's only now and then): a
        // present they have not shown a refresh after it would have been is estimated.
        let grace = if self.mode == Mode::Estimated {
            0
        } else {
            period
        };
        self.estimate(pending, &poll, grace, &mut out);
        self.last_poll = Some(poll.now);
        out
    }

    fn time_of(&self, refresh: u32, anchor: (u32, i64), period: i64) -> (i64, bool) {
        match self.vblanks.iter().find(|(r, _)| *r == refresh) {
            Some(&(_, at)) => (at, true),
            None => (anchor.1 + offset(refresh, anchor.0) * period, false),
        }
    }

    fn exact(
        &mut self,
        pending: &mut VecDeque<Pending>,
        s: Stats,
        period: i64,
        out: &mut Vec<(u64, Outcome)>,
    ) {
        let anchor = (s.sync_refresh, s.sync_qpc);
        let upto = pending
            .iter()
            .take_while(|p| !newer(p.count, s.present_count))
            .count();
        if upto == 0 {
            return;
        }
        let floor = self.shown.map(|(_, r)| r);
        let resolved: Vec<Pending> = pending.drain(..upto).collect();
        // A present missed between two polls: shown, if at all, on the refresh after the first
        // vblank following it, unless a later one took that refresh too.
        let slot = |p: &Pending| {
            let since = anchor.1 - p.ready.unwrap_or(p.queued).max(p.queued);
            let first = anchor.0.wrapping_add((1 - ceil_div(since, period)) as u32);
            let slot = first.wrapping_add(1);
            match floor {
                Some(floor) if !newer(slot, floor) => floor.wrapping_add(1),
                _ => slot,
            }
        };
        let refresh_of = |p: &Pending| {
            if p.count == s.present_count {
                s.present_refresh
            } else {
                slot(p)
            }
        };
        for (i, p) in resolved.iter().enumerate() {
            if p.count == s.present_count {
                let (at, exact) = self.time_of(s.present_refresh, anchor, period);
                out.push((
                    p.present,
                    self.displayed(s.present_refresh, at, exact, period),
                ));
                continue;
            }
            let mine = slot(p);
            let taken = resolved
                .get(i + 1)
                .is_some_and(|next| !newer(refresh_of(next), mine));
            if taken || !newer(s.present_refresh, mine) {
                out.push((p.present, Outcome::Discarded));
            } else {
                let (at, _) = self.time_of(mine, anchor, period);
                out.push((p.present, self.displayed(mine, at, false, period)));
            }
        }
        self.shown = Some((s.present_count, s.present_refresh));
    }

    fn displayed(&self, refresh: u32, at: i64, exact: bool, period: i64) -> Outcome {
        let before = self
            .vblanks
            .iter()
            .find(|(r, _)| *r == refresh.wrapping_sub(1));
        let interval = match before {
            Some(&(_, prev)) if exact => at - prev,
            _ => period,
        };
        Outcome::Displayed {
            at,
            refresh: interval,
            exact,
        }
    }

    fn estimate(
        &mut self,
        pending: &mut VecDeque<Pending>,
        poll: &Poll,
        grace: i64,
        out: &mut Vec<(u64, Outcome)>,
    ) {
        let period = poll.period.max(1);
        let slot = |ready: i64| poll.vblank + ceil_div(ready + 1 - poll.vblank, period) * period;
        while let Some(front) = pending.front() {
            let Some(ready) = front.ready else { break };
            let mine = slot(ready);
            let at = mine + period;
            if at + grace > poll.now {
                break;
            }
            let taken = pending
                .get(1)
                .is_some_and(|next| next.ready.is_some_and(|r| slot(r) == mine));
            let present = front.present;
            pending.pop_front();
            out.push((
                present,
                if taken {
                    Outcome::Discarded
                } else {
                    Outcome::Displayed {
                        at,
                        refresh: period,
                        exact: false,
                    }
                },
            ));
        }
    }
}

struct Chain {
    hwnd: isize,
    swap_chain: IDXGISwapChain,
    fence: Option<ID3D12Fence>,
    next: u64,
    pending: VecDeque<Pending>,
    decoder: Decoder,
}

// SAFETY: DXGI swap chains and D3D12 fences are free-threaded; the chain is only reached under
// `CHAINS`' lock.
unsafe impl Send for Chain {}

static CHAINS: Mutex<Vec<Chain>> = Mutex::new(Vec::new());

fn chains() -> std::sync::MutexGuard<'static, Vec<Chain>> {
    CHAINS.lock().unwrap_or_else(|e| e.into_inner())
}

static FREQUENCY: LazyLock<i64> = LazyLock::new(|| {
    let mut f = 0;
    // SAFETY: always succeeds on XP and later.
    unsafe { QueryPerformanceFrequency(&mut f).ok() };
    f.max(1)
});

fn qpc() -> i64 {
    let mut now = 0;
    // SAFETY: always succeeds on XP and later.
    unsafe { QueryPerformanceCounter(&mut now).ok() };
    now
}

/// QPC ticks to nanoseconds, as mpv's `mp_raw_time_ns` converts them.
pub(crate) fn qpc_ns(ticks: i64) -> u64 {
    let f = *FREQUENCY;
    let ticks = ticks.max(0);
    ((ticks / f) * 1_000_000_000 + (ticks % f) * 1_000_000_000 / f) as u64
}

fn ticks_ns(ticks: i64) -> u64 {
    (ticks.max(0) as i128 * 1_000_000_000 / *FREQUENCY as i128) as u64
}

fn to_platform(outcome: Outcome) -> PresentOutcome {
    match outcome {
        Outcome::Displayed { at, refresh, exact } => PresentOutcome::Displayed {
            at_ns: qpc_ns(at),
            refresh_ns: ticks_ns(refresh),
            exact,
        },
        Outcome::Discarded => PresentOutcome::Discarded,
    }
}

/// Now, and the compositor's next vblank and refresh period, in QPC ticks.
fn clock() -> (i64, i64, i64) {
    let mut timing = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a sized struct; a null window asks for the compositor's own timing.
    let (vblank, period) =
        match unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing) } {
            Ok(()) => (timing.qpcVBlank as i64, timing.qpcRefreshPeriod as i64),
            Err(_) => (0, 0),
        };
    let now = qpc();
    let period = if period > 0 { period } else { *FREQUENCY / 60 };
    (now, if vblank > 0 { vblank } else { now }, period)
}

/// Reads `chain`'s statistics and settles what they show.
fn sample(chain: &mut Chain, (now, vblank, period): (i64, i64, i64)) -> Vec<(u64, Outcome)> {
    if chain.pending.is_empty() {
        return Vec::new();
    }
    let mut raw = DXGI_FRAME_STATISTICS::default();
    // SAFETY: a live swap chain; statistics are read-only.
    let stats = unsafe { chain.swap_chain.GetFrameStatistics(&mut raw) }
        .ok()
        .map(|()| Stats {
            present_count: raw.PresentCount,
            present_refresh: raw.PresentRefreshCount,
            sync_refresh: raw.SyncRefreshCount,
            sync_qpc: raw.SyncQPCTime,
        });
    let fence = chain.fence.clone();
    let done = |value: u64| {
        fence
            .as_ref()
            // SAFETY: a live fence.
            .is_none_or(|f| unsafe { f.GetCompletedValue() } >= value)
    };
    let poll = Poll {
        now,
        period,
        vblank,
        stats,
    };
    chain.decoder.decode(&mut chain.pending, poll, done)
}

/// After a successful `Present` of `hwnd`'s swap chain, whose GPU work signals `fence`. The
/// statistics are read here too: a present shown before this one was queued is then seen
/// exactly, even when the vsync thread missed its refresh.
pub(crate) fn presented(
    hwnd: HWND,
    swap_chain: &IDXGISwapChain,
    fence: Option<(&ID3D12Fence, u64)>,
) {
    if !present_feedback_wanted() {
        return;
    }
    let Some(listener) = present_listener() else {
        return;
    };
    // SAFETY: a live swap chain.
    let Ok(count) = (unsafe { swap_chain.GetLastPresentCount() }) else {
        return;
    };
    let queued = qpc();
    let key = hwnd.0 as isize;
    let (present, settled) = {
        let mut chains = chains();
        let mut settled = Vec::new();
        let index = match chains.iter().position(|c| c.hwnd == key) {
            Some(i) if chains[i].swap_chain.as_raw() == swap_chain.as_raw() => i,
            found => {
                if let Some(i) = found {
                    let old = chains.swap_remove(i);
                    settled.extend(old.pending.iter().map(|p| (p.present, Outcome::Discarded)));
                }
                chains.push(Chain {
                    hwnd: key,
                    swap_chain: swap_chain.clone(),
                    fence: None,
                    next: 0,
                    pending: VecDeque::new(),
                    decoder: Decoder::default(),
                });
                chains.len() - 1
            }
        };
        let chain = &mut chains[index];
        settled.extend(sample(chain, clock()));
        chain.fence = fence.map(|(f, _)| f.clone());
        chain.next += 1;
        (chain.next, settled)
    };
    for (old, outcome) in settled {
        listener.resolved(key as u64, old, to_platform(outcome));
    }
    listener.queued(key as u64, present);
    if let Some(chain) = chains().iter_mut().find(|c| c.hwnd == key) {
        chain.pending.push_back(Pending {
            present,
            count,
            queued,
            fence: fence.map_or(0, |(_, v)| v),
            ready: None,
        });
    }
}

/// Before `hwnd`'s swap chain is released: an HWND can own only one flip-model swap chain, so
/// none may be kept alive here.
pub(crate) fn forget(hwnd: HWND) {
    let gone: Vec<u64> = {
        let mut chains = chains();
        let mut gone = Vec::new();
        chains.retain(|c| {
            let keep = c.hwnd != hwnd.0 as isize;
            if !keep {
                gone.extend(c.pending.iter().map(|p| p.present));
            }
            keep
        });
        gone
    };
    if let Some(listener) = present_listener() {
        for present in gone {
            listener.resolved(hwnd.0 as u64, present, PresentOutcome::Discarded);
        }
    }
}

/// On the vsync thread, once per refresh.
pub(crate) fn poll() {
    if !present_feedback_wanted() {
        return;
    }
    let Some(listener) = present_listener() else {
        return;
    };
    let clock = clock();
    let mut outcomes = Vec::new();
    for chain in chains().iter_mut() {
        let hwnd = chain.hwnd as u64;
        outcomes.extend(sample(chain, clock).into_iter().map(|(p, o)| (hwnd, p, o)));
    }
    for (hwnd, present, outcome) in outcomes {
        listener.resolved(hwnd, present, to_platform(outcome));
    }
    listener.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 1000;

    fn pending(present: u64, count: u32, queued: i64) -> Pending {
        Pending {
            present,
            count,
            queued,
            fence: 0,
            ready: None,
        }
    }

    /// Statistics sampled on refresh `sync` (at `sync * P`), the last shown present `count` on `on`.
    fn poll(sync: u32, count: u32, on: u32) -> Poll {
        Poll {
            now: sync as i64 * P + 10,
            period: P,
            vblank: sync as i64 * P,
            stats: Some(Stats {
                present_count: count,
                present_refresh: on,
                sync_refresh: sync,
                sync_qpc: sync as i64 * P,
            }),
        }
    }

    fn shown(at_refresh: i64, exact: bool) -> Outcome {
        Outcome::Displayed {
            at: at_refresh * P,
            refresh: P,
            exact,
        }
    }

    #[test]
    fn each_present_resolves_once_on_the_refresh_it_was_first_shown() {
        let mut d = Decoder::default();
        let mut q = VecDeque::from([pending(1, 10, 99 * P + 500)]);
        assert!(
            d.decode(&mut q, poll(100, 9, 99), |_| true).is_empty(),
            "not shown yet"
        );
        q.push_back(pending(2, 11, 100 * P + 500));
        assert_eq!(
            d.decode(&mut q, poll(101, 10, 101), |_| true),
            [(1, shown(101, true))]
        );
        assert_eq!(
            d.decode(&mut q, poll(102, 11, 102), |_| true),
            [(2, shown(102, true))]
        );
        assert!(q.is_empty());
        assert!(d.decode(&mut q, poll(103, 11, 102), |_| true).is_empty());
    }

    #[test]
    fn a_present_superseded_before_its_refresh_is_discarded() {
        let mut d = Decoder::default();
        let mut q = VecDeque::from([pending(1, 1, 99 * P + 100)]);
        assert_eq!(
            d.decode(&mut q, poll(100, 1, 100), |_| true),
            [(1, shown(100, true))]
        );
        // Two presents within one refresh: DWM shows only the second.
        q.extend([pending(2, 2, 100 * P + 100), pending(3, 3, 100 * P + 600)]);
        assert_eq!(
            d.decode(&mut q, poll(102, 3, 102), |_| true),
            [(2, Outcome::Discarded), (3, shown(102, true))]
        );
    }

    #[test]
    fn a_missed_poll_still_places_a_present_that_had_a_refresh_of_its_own() {
        let mut d = Decoder::default();
        let mut q = VecDeque::from([pending(1, 1, 99 * P + 100)]);
        d.decode(&mut q, poll(100, 1, 100), |_| true);
        // Polls on 101..103 were missed; present 2 had refresh 102 to itself, 3 took 104.
        q.extend([pending(2, 2, 100 * P + 300), pending(3, 3, 102 * P + 300)]);
        let out = d.decode(&mut q, poll(104, 3, 104), |_| true);
        assert_eq!(out, [(2, shown(102, false)), (3, shown(104, true))]);
        // A present sampled late, after its own refresh, is placed from the refresh counts.
        q.push_back(pending(4, 4, 104 * P + 300));
        let out = d.decode(&mut q, poll(108, 4, 106), |_| true);
        assert_eq!(out, [(4, shown(106, false))]);
    }

    #[test]
    fn a_present_the_statistics_lag_behind_is_estimated_a_refresh_after_its_own() {
        let mut d = Decoder::default();
        let mut q = VecDeque::from([pending(1, 1, 99 * P + 100)]);
        d.decode(&mut q, poll(100, 1, 100), |_| true);
        q.push_back(pending(2, 2, 100 * P + 300));
        let stale = |now: i64| Poll {
            now,
            vblank: now - now % P,
            ..poll(100, 1, 100)
        };
        assert!(d.decode(&mut q, stale(102 * P + 10), |_| true).is_empty());
        assert_eq!(
            d.decode(&mut q, stale(103 * P + 10), |_| true),
            [(2, shown(102, false))]
        );
    }

    #[test]
    fn a_present_left_no_refresh_between_its_neighbours_was_discarded() {
        let mut d = Decoder::default();
        let mut q = VecDeque::from([pending(1, 1, 99 * P + 100)]);
        d.decode(&mut q, poll(100, 1, 100), |_| true);
        q.extend([pending(2, 2, 100 * P + 300), pending(3, 3, 100 * P + 400)]);
        let out = d.decode(&mut q, poll(103, 3, 101), |_| true);
        assert_eq!(out, [(2, Outcome::Discarded), (3, shown(101, false))]);
    }

    #[test]
    fn a_present_made_while_untracked_still_anchors_the_ones_before_it() {
        let mut d = Decoder::default();
        // Present count 2 was not recorded; 1 had refresh 101 before it.
        let mut q = VecDeque::from([pending(1, 1, 99 * P + 100)]);
        assert_eq!(
            d.decode(&mut q, poll(103, 2, 102), |_| true),
            [(1, shown(101, false))]
        );
    }

    #[test]
    fn without_statistics_presents_are_estimated_and_superseded_ones_discarded() {
        let mut d = Decoder::default();
        let empty = |now: i64| Poll {
            now,
            period: P,
            vblank: now - now % P,
            stats: None,
        };
        let mut q = VecDeque::from([pending(1, 1, 100 * P + 100)]);
        assert!(d.decode(&mut q, empty(101 * P + 10), |_| false).is_empty());
        // Statistics never came: estimated from then on, once the GPU is done.
        assert!(d.decode(&mut q, empty(110 * P + 10), |_| false).is_empty());
        assert!(
            d.decode(&mut q, empty(111 * P + 10), |_| true).is_empty(),
            "the GPU finished after the last poll: composed on 111, shown on 112"
        );
        let out = d.decode(&mut q, empty(112 * P + 10), |_| true);
        assert_eq!(out, [(1, shown(112, false))]);
        q.extend([pending(2, 2, 112 * P + 100), pending(3, 3, 112 * P + 200)]);
        let out = d.decode(&mut q, empty(114 * P + 10), |_| true);
        assert_eq!(out, [(2, Outcome::Discarded), (3, shown(114, false))]);
    }

    #[test]
    fn the_refresh_interval_is_measured_between_sampled_vblanks() {
        let mut d = Decoder::default();
        let at = |sync: u32, qpc: i64, count: u32| Poll {
            now: qpc + 10,
            period: P,
            vblank: qpc,
            stats: Some(Stats {
                present_count: count,
                present_refresh: sync,
                sync_refresh: sync,
                sync_qpc: qpc,
            }),
        };
        let mut q = VecDeque::from([pending(1, 1, 9_500)]);
        assert!(d.decode(&mut q, at(10, 10_000, 0), |_| true).is_empty());
        let out = d.decode(&mut q, at(11, 11_700, 1), |_| true);
        assert_eq!(
            out,
            [(
                1,
                Outcome::Displayed {
                    at: 11_700,
                    refresh: 1_700,
                    exact: true
                }
            )]
        );
    }
}
