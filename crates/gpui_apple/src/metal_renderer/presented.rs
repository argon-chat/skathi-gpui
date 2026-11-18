//! vue-native: what became of each drawable a window presented, for `gpui::present_feedback`.
//! macOS can hand a drawable that reached the screen back with a `presentedTime` of 0, and can run
//! a presented handler late or not at all. So a drawable without a time counts as replaced only
//! when a later present aimed at the same refresh; otherwise it was shown, at the refresh it aimed
//! at.

use std::collections::VecDeque;

use gpui::present_feedback::PresentOutcome;

struct Pending {
    present: u64,
    /// When the drawable was to reach the screen, in host-time nanoseconds.
    target: u64,
    /// The target of the window's next present, once there is one.
    next: Option<u64>,
}

#[derive(Default)]
pub(super) struct Presents {
    pending: VecDeque<Pending>,
}

impl Presents {
    /// Every present of the window, reported or not, aimed at `target`.
    pub(super) fn presenting(&mut self, target: u64) {
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
        });
    }

    /// `present`'s handler ran with `presented_at` (0 without a time): the outcomes of it and of
    /// every earlier present still waiting, in present order.
    pub(super) fn presented(
        &mut self,
        present: u64,
        presented_at: u64,
        refresh_ns: u64,
    ) -> Vec<(u64, PresentOutcome)> {
        let mut outcomes = Vec::new();
        while self.pending.front().is_some_and(|p| p.present <= present) {
            let p = self.pending.pop_front().expect("looked at");
            let outcome = if p.present == present && presented_at > 0 {
                PresentOutcome::Displayed {
                    at_ns: presented_at,
                    refresh_ns: 0,
                    exact: true,
                }
            } else if p.next.is_some_and(|next| next < p.target + refresh_ns / 2) {
                PresentOutcome::Discarded
            } else {
                PresentOutcome::Displayed {
                    at_ns: p.target,
                    refresh_ns: 0,
                    exact: false,
                }
            };
            outcomes.push((p.present, outcome));
        }
        outcomes
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
        log.presenting(target);
        log.queued(present, target);
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
            assert_eq!(log.presented(i, at, REFRESH), [(i, expected)]);
        }
    }

    #[test]
    fn a_drawable_replaced_on_its_refresh_is_discarded_and_one_shown_before_the_next_is_not() {
        let mut log = Presents::default();
        present(&mut log, 1, 100 * MS);
        present(&mut log, 2, 100 * MS + REFRESH / 4);
        present(&mut log, 3, 100 * MS + 2 * REFRESH);
        assert_eq!(
            log.presented(1, 0, REFRESH),
            [(1, PresentOutcome::Discarded)]
        );
        assert_eq!(
            log.presented(2, 0, REFRESH),
            [(2, shown(100 * MS + REFRESH / 4, false))]
        );
    }

    /// A live resize presents with the window's transaction, between display-link frames, and
    /// the handlers of those drawables may come back without a time, late, or not at all.
    #[test]
    fn a_live_resize_reports_every_drawable_in_present_order() {
        let mut log = Presents::default();
        let mut heard = Vec::new();
        for i in 1..=60u64 {
            let target = 1_000 * MS + i * 16 * MS + REFRESH;
            if i % 7 == 0 {
                log.presenting(target);
                continue;
            }
            present(&mut log, i, target);
            match i % 4 {
                0 => {}
                1 => heard.extend(log.presented(i, target + MS, REFRESH)),
                _ => heard.extend(log.presented(i, 0, REFRESH)),
            }
        }
        heard.extend(log.presented(60, 0, REFRESH));
        let presents: Vec<u64> = heard.iter().map(|&(p, _)| p).collect();
        assert_eq!(
            presents,
            (1..=60).filter(|i| i % 7 != 0).collect::<Vec<_>>()
        );
        for (p, outcome) in heard {
            let target = 1_000 * MS + p * 16 * MS + REFRESH;
            let at = match outcome {
                PresentOutcome::Displayed { at_ns, .. } => at_ns,
                PresentOutcome::Discarded => panic!("present {p} was not replaced"),
            };
            assert!(at == target || at == target + MS, "present {p} at {at}");
        }
    }
}
