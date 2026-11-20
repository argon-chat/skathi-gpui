use std::time::{Duration, Instant};

const PLAYBACK_GAP: Duration = Duration::from_millis(64);
const PAUSE_GAP: Duration = Duration::from_millis(125);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pacing {
    Immediate,
    DisplayLink,
}

#[derive(Default)]
pub(super) struct Cadence {
    last: Option<Instant>,
    short_gaps: u8,
    pub(super) pending: Option<Pacing>,
}

impl Cadence {
    pub(super) fn ready(&mut self, now: Instant) -> bool {
        let gap = self.last.replace(now).map(|t| now.duration_since(t));
        let streaming = self.short_gaps == 2;
        let isolated = gap.is_none_or(|d| d >= if streaming { PAUSE_GAP } else { PLAYBACK_GAP });
        self.short_gaps = if isolated {
            0
        } else if gap.is_some_and(|d| d < PLAYBACK_GAP) {
            (self.short_gaps + 1).min(2)
        } else {
            self.short_gaps
        };
        let was_pending = self.pending.is_some();
        self.pending = Some(
            if isolated || (self.pending == Some(Pacing::Immediate) && self.short_gaps < 2) {
                Pacing::Immediate
            } else {
                Pacing::DisplayLink
            },
        );
        !was_pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(clock: &mut Cadence, start: Instant, ms: u64) -> Pacing {
        assert!(clock.ready(start + Duration::from_millis(ms)));
        clock.pending.take().unwrap()
    }

    #[test]
    fn playback_uses_the_display_grid_and_the_first_step_after_a_pause_is_immediate() {
        for fps in [24, 60, 120] {
            let start = Instant::now();
            let mut clock = Cadence::default();
            assert_eq!(ready(&mut clock, start, 0), Pacing::Immediate);
            for n in 1..=fps {
                assert_eq!(
                    ready(&mut clock, start, n * 1000 / fps),
                    Pacing::DisplayLink
                );
            }
            assert_eq!(ready(&mut clock, start, 1124), Pacing::DisplayLink);
            assert_eq!(ready(&mut clock, start, 1249), Pacing::Immediate);
            assert_eq!(ready(&mut clock, start, 1313), Pacing::Immediate);
        }
    }

    #[test]
    fn redraws_coalesce_and_a_stalled_ui_resumes_a_train_on_the_display_link() {
        let start = Instant::now();
        let mut clock = Cadence::default();
        assert!(clock.ready(start));
        assert!(!clock.ready(start + Duration::from_micros(6800)));
        assert_eq!(clock.pending.take(), Some(Pacing::Immediate));
        assert_eq!(ready(&mut clock, start, 100), Pacing::Immediate);
        assert!(clock.ready(start + Duration::from_millis(300)));
        assert!(!clock.ready(start + Duration::from_millis(340)));
        assert!(!clock.ready(start + Duration::from_millis(380)));
        assert_eq!(clock.pending.take(), Some(Pacing::DisplayLink));
    }
}
