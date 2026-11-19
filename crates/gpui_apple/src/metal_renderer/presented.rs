//! vue-native: what became of each drawable a window presented, for `gpui::present_feedback`.
//! macOS can hand a drawable that reached the screen back with a `presentedTime` of 0, and can run
//! a presented handler late or not at all. So a drawable without a time counts as replaced when a
//! later present aimed at the same refresh, or, at one present per refresh, when the next one was
//! shown exactly at the refresh it aimed at; otherwise it was shown, at the refresh it aimed at.
//! An estimated outcome is held until the time it reports, and outcomes go out in present order.

use std::collections::VecDeque;

use gpui::present_feedback::PresentOutcome;

/// How long a drawable without a time waits for the next present's handler, after that present's
/// target, before it is taken as shown.
const EVIDENCE_REFRESHES: u64 = 2;
/// A drawable drawn outside the display link (live resize, a window becoming key) presents with
/// the window's transaction and is shown on the first vblank at least this many refreshes later.
const TRANSACTION_REFRESHES: u64 = 2;
/// How long a vblank stays a usable anchor for the refresh grid.
const ANCHOR_NS: u64 = 1_000_000_000;

struct Pending {
    present: u64,
    /// When the drawable was to reach the screen, in host-time nanoseconds.
    target: u64,
    /// The target of the window's next present, reported or not, once there is one.
    next: Option<u64>,
    /// The `presentedTime` its handler saw, 0 without one.
    heard: Option<u64>,
}

struct Ready {
    present: u64,
    outcome: PresentOutcome,
    due: u64,
    timeless: bool,
}

#[derive(Default)]
pub(super) struct Presents {
    pending: VecDeque<Pending>,
    ready: VecDeque<Ready>,
    /// The latest present whose handler ran.
    heard_upto: u64,
    refresh: u64,
    /// A known vblank: a display link's target or a shown drawable's time.
    anchor: Option<u64>,
    /// When a scheduled settle runs.
    timer: Option<u64>,
}

impl Presents {
    /// Every present of the window, reported or not, aimed at `target`.
    pub(super) fn presenting(&mut self, target: u64, refresh: u64) {
        self.refresh = refresh.max(1);
        if let Some(last) = self.pending.back_mut() {
            last.next.get_or_insert(target);
        }
    }

    /// A present that is reported as `present`, after [`Self::presenting`].
    pub(super) fn queued(&mut self, present: u64, target: u64) {
        self.pending.push_back(Pending {
            present,
            target,
            next: None,
            heard: None,
        });
    }

    /// A display link's target: a vblank on the screen's grid.
    pub(super) fn anchor(&mut self, vblank: u64) {
        self.anchor = Some(vblank);
    }

    /// Where a drawable presented with the window's transaction at `now` is shown.
    pub(super) fn transaction_target(&self, now: u64, refresh: u64) -> u64 {
        let r = refresh.max(1);
        let earliest = now + TRANSACTION_REFRESHES * r;
        match self.anchor.filter(|a| a.abs_diff(now) < ANCHOR_NS) {
            Some(a) if a >= earliest => a - (a - earliest) / r * r,
            Some(a) => a + (earliest - a).div_ceil(r) * r,
            None => earliest + r / 2,
        }
    }

    /// `present`'s handler ran, with `presented_at` (0 without a time).
    pub(super) fn presented(&mut self, present: u64, presented_at: u64) {
        if let Some(p) = self.pending.iter_mut().find(|p| p.present == present) {
            p.heard = Some(presented_at);
        }
        if presented_at > 0 {
            self.anchor = Some(presented_at);
        }
        self.heard_upto = self.heard_upto.max(present);
    }

    /// The outcomes that can go out at `now`, in present order.
    pub(super) fn settle(&mut self, now: u64) -> Vec<(u64, PresentOutcome, bool)> {
        let r = self.refresh.max(1);
        while let Some(p) = self.pending.front() {
            if p.present > self.heard_upto {
                break;
            }
            let estimated = PresentOutcome::Displayed {
                at_ns: p.target,
                refresh_ns: 0,
                exact: false,
            };
            let outcome = match p.heard {
                Some(at) if at > 0 => PresentOutcome::Displayed {
                    at_ns: at,
                    refresh_ns: 0,
                    exact: true,
                },
                _ if p.next.is_some_and(|next| next < p.target + r / 2) => {
                    PresentOutcome::Discarded
                }
                _ => match self
                    .pending
                    .get(1)
                    .filter(|n| Some(n.target) == p.next && n.target < p.target + r + r / 2)
                {
                    None => estimated,
                    Some(next) => match next.heard {
                        Some(at) if at > 0 && at < next.target + r / 2 => PresentOutcome::Discarded,
                        Some(_) => estimated,
                        None if next.present <= self.heard_upto => estimated,
                        None if now >= next.target + EVIDENCE_REFRESHES * r => estimated,
                        None => break,
                    },
                },
            };
            let due = match outcome {
                PresentOutcome::Displayed { at_ns, exact, .. } if !exact => at_ns,
                _ => 0,
            };
            let timeless = p.heard.is_none_or(|at| at == 0);
            let present = p.present;
            self.pending.pop_front();
            self.ready.push_back(Ready {
                present,
                outcome,
                due,
                timeless,
            });
        }
        let mut out = Vec::new();
        while let Some(r) = self.ready.front().filter(|r| r.due <= now) {
            out.push((r.present, r.outcome, r.timeless));
            self.ready.pop_front();
        }
        out
    }

    /// When to settle again for what is held, unless a settle is already scheduled by then.
    pub(super) fn wake_at(&mut self) -> Option<u64> {
        let r = self.refresh.max(1);
        let held = self.ready.front().map(|r| r.due);
        let waiting = match (self.pending.front(), self.pending.get(1)) {
            (Some(p), Some(n)) if p.present <= self.heard_upto => {
                Some(n.target + EVIDENCE_REFRESHES * r)
            }
            _ => None,
        };
        let at = held.into_iter().chain(waiting).min()?;
        if self.timer.is_some_and(|t| t <= at) {
            return None;
        }
        self.timer = Some(at);
        Some(at)
    }

    /// The settle scheduled for `at` ran.
    pub(super) fn woke(&mut self, at: u64) {
        if self.timer == Some(at) {
            self.timer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFRESH: u64 = 8_333_333;
    const MS: u64 = 1_000_000;

    fn shown(at_ns: u64, exact: bool) -> PresentOutcome {
        PresentOutcome::Displayed {
            at_ns,
            refresh_ns: 0,
            exact,
        }
    }

    fn present(log: &mut Presents, present: u64, target: u64) {
        log.presenting(target, REFRESH);
        log.queued(present, target);
    }

    fn heard(log: &mut Presents, present: u64, at: u64, now: u64) -> Vec<(u64, PresentOutcome)> {
        log.presented(present, at);
        log.settle(now)
            .into_iter()
            .map(|(p, o, _)| (p, o))
            .collect()
    }

    #[test]
    fn sparse_drawables_without_a_time_were_shown_at_the_refresh_they_aimed_at() {
        let mut log = Presents::default();
        for (i, at) in [(1, 0), (2, 0), (3, 1_530 * MS), (4, 0)] {
            let target = i * 500 * MS + 3 * REFRESH;
            present(&mut log, i, target);
            let expected = if at > 0 {
                shown(at, true)
            } else {
                shown(target, false)
            };
            assert_eq!(heard(&mut log, i, at, target.max(at)), [(i, expected)]);
        }
    }

    #[test]
    fn an_estimate_goes_out_no_sooner_than_the_time_it_reports() {
        let mut log = Presents::default();
        let target = 500 * MS;
        present(&mut log, 1, target);
        assert!(heard(&mut log, 1, 0, target - 38 * MS).is_empty());
        assert_eq!(log.wake_at(), Some(target));
        assert_eq!(log.wake_at(), None, "already scheduled");
        present(&mut log, 2, target + 600 * MS);
        assert!(
            heard(&mut log, 2, target + 600 * MS, target - MS).is_empty(),
            "a later outcome waits behind it"
        );
        log.woke(target);
        assert_eq!(
            log.settle(target),
            [
                (1, shown(target, false), true),
                (2, shown(target + 600 * MS, true), false)
            ]
        );
    }

    #[test]
    fn at_one_present_per_refresh_a_drawable_without_a_time_before_one_shown_on_time_was_skipped() {
        let mut log = Presents::default();
        let target = |i: u64| 1_000 * MS + i * REFRESH;
        let mut heard_all = Vec::new();
        for i in 1..=10 {
            present(&mut log, i, target(i));
            if i >= 2 {
                let at = if i - 1 == 5 { 0 } else { target(i - 1) };
                heard_all.extend(heard(&mut log, i - 1, at, target(i - 1) + MS));
            }
        }
        heard_all.extend(heard(&mut log, 10, target(10), target(10) + MS));
        let expected: Vec<_> = (1..=10)
            .map(|i| {
                let o = if i == 5 {
                    PresentOutcome::Discarded
                } else {
                    shown(target(i), true)
                };
                (i, o)
            })
            .collect();
        assert_eq!(heard_all, expected);
    }

    #[test]
    fn at_one_present_per_refresh_a_drawable_without_a_time_before_a_late_one_was_shown() {
        let mut log = Presents::default();
        let target = |i: u64| 1_000 * MS + i * REFRESH;
        present(&mut log, 1, target(1));
        present(&mut log, 2, target(2));
        assert!(heard(&mut log, 1, 0, target(1) + MS).is_empty());
        assert_eq!(
            heard(&mut log, 2, target(3), target(3) + MS),
            [(1, shown(target(1), false)), (2, shown(target(3), true))]
        );
    }

    #[test]
    fn a_drawable_without_a_time_is_taken_as_shown_when_the_next_handler_never_runs() {
        let mut log = Presents::default();
        let target = |i: u64| 1_000 * MS + i * REFRESH;
        present(&mut log, 1, target(1));
        present(&mut log, 2, target(2));
        assert!(heard(&mut log, 1, 0, target(1) + MS).is_empty());
        let deadline = target(2) + EVIDENCE_REFRESHES * REFRESH;
        assert_eq!(log.wake_at(), Some(deadline));
        log.woke(deadline);
        assert_eq!(log.settle(deadline), [(1, shown(target(1), false), true)]);
    }

    #[test]
    fn a_stepped_frame_is_shown_unless_a_redraw_replaced_it_on_its_refresh() {
        let mut log = Presents::default();
        let mut out = Vec::new();
        for step in 0..3u64 {
            let target = (1_000 + step * 400) * MS;
            present(&mut log, 2 * step + 1, target);
            if step == 1 {
                present(&mut log, 2 * step + 2, target + 2 * MS);
                out.extend(heard(&mut log, 2 * step + 1, 0, target));
                out.extend(heard(&mut log, 2 * step + 2, 0, target + 2 * MS));
            } else {
                out.extend(heard(&mut log, 2 * step + 1, 0, target));
            }
        }
        assert_eq!(
            out,
            [
                (1, shown(1_000 * MS, false)),
                (3, PresentOutcome::Discarded),
                (4, shown(1_402 * MS, false)),
                (5, shown(1_800 * MS, false)),
            ]
        );
    }

    #[test]
    fn a_drawable_replaced_on_its_refresh_is_discarded_and_one_shown_before_the_next_is_not() {
        let mut log = Presents::default();
        present(&mut log, 1, 100 * MS);
        present(&mut log, 2, 100 * MS + REFRESH / 4);
        present(&mut log, 3, 100 * MS + 3 * REFRESH);
        assert_eq!(
            heard(&mut log, 1, 0, 100 * MS),
            [(1, PresentOutcome::Discarded)]
        );
        assert_eq!(
            heard(&mut log, 2, 0, 100 * MS + REFRESH / 4),
            [(2, shown(100 * MS + REFRESH / 4, false))]
        );
    }

    #[test]
    fn a_transaction_present_is_placed_on_the_grid_two_refreshes_out() {
        let mut log = Presents::default();
        let vblank = 1_000 * MS;
        log.anchor(vblank);
        for now in [vblank + MS, vblank + 5 * MS, vblank + REFRESH] {
            let at = log.transaction_target(now, REFRESH);
            assert_eq!((at - vblank) % REFRESH, 0, "on the grid");
            assert!(at >= now + 2 * REFRESH && at < now + 3 * REFRESH);
        }
        log.anchor(vblank + 10 * REFRESH);
        assert_eq!(
            log.transaction_target(vblank + MS, REFRESH),
            vblank + 3 * REFRESH
        );
    }

    /// A live resize presents with the window's transaction, between display-link frames, and
    /// the handlers of those drawables may come back without a time, late, or not at all.
    #[test]
    fn a_live_resize_reports_every_drawable_in_present_order() {
        let mut log = Presents::default();
        let mut out = Vec::new();
        for i in 1..=60u64 {
            let target = 1_000 * MS + i * 16 * MS + REFRESH;
            if i % 7 == 0 {
                log.presenting(target, REFRESH);
                continue;
            }
            present(&mut log, i, target);
            match i % 4 {
                0 => {}
                1 => out.extend(heard(&mut log, i, target + MS, target + MS)),
                _ => out.extend(heard(&mut log, i, 0, target)),
            }
        }
        out.extend(heard(&mut log, 60, 0, 2_000 * MS));
        let presents: Vec<u64> = out.iter().map(|&(p, _)| p).collect();
        assert_eq!(
            presents,
            (1..=60).filter(|i| i % 7 != 0).collect::<Vec<_>>()
        );
        for (p, outcome) in out {
            let target = 1_000 * MS + p * 16 * MS + REFRESH;
            let at = match outcome {
                PresentOutcome::Displayed { at_ns, .. } => at_ns,
                PresentOutcome::Discarded => panic!("present {p} was not replaced"),
            };
            assert!(at == target || at == target + MS, "present {p} at {at}");
        }
    }
}
