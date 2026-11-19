use std::cell::Cell;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct TimerFrames {
    interval: Cell<Option<Duration>>,
    last: Cell<Option<Instant>>,
    pending: Cell<bool>,
    generation: Cell<u64>,
}

impl TimerFrames {
    pub(super) fn start(&self, interval: Duration) {
        self.interval.set(Some(interval));
    }

    pub(super) fn stop(&self) {
        self.interval.set(None);
        self.last.set(None);
        self.pending.set(false);
        self.generation.set(self.generation.get().wrapping_add(1));
    }

    pub(super) fn next(&self, now: Instant) -> Option<(u64, Duration)> {
        let interval = self.interval.get()?;
        if self.pending.replace(true) {
            return None;
        }
        let delay = self.last.get().map_or(Duration::ZERO, |last| {
            (last + interval).saturating_duration_since(now)
        });
        Some((self.generation.get(), delay))
    }

    pub(super) fn fire(&self, generation: u64, now: Instant) -> bool {
        if self.generation.get() != generation || !self.pending.replace(false) {
            return false;
        }
        self.last.set(Some(now));
        true
    }

    pub(super) fn debug_state(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "LocalTimer",
            "active": self.interval.get().is_some(),
            "pending": self.pending.get(),
            "interval_ns": self.interval.get().map(|i| i.as_nanos() as u64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_frames_are_immediate_and_continuous_demand_is_coalesced() {
        let timer = TimerFrames::default();
        let now = Instant::now();
        let interval = Duration::from_millis(8);
        assert_eq!(timer.next(now), None);
        timer.start(interval);
        let (generation, delay) = timer.next(now).unwrap();
        assert_eq!(delay, Duration::ZERO);
        assert_eq!(timer.next(now), None);
        assert!(timer.fire(generation, now));
        assert_eq!(
            timer.next(now + interval / 4),
            Some((generation, interval * 3 / 4))
        );
        assert!(timer.fire(generation, now + interval));
        assert_eq!(
            timer.next(now + interval * 10),
            Some((generation, Duration::ZERO))
        );
    }

    #[test]
    fn a_callback_queued_before_hide_cannot_consume_a_new_frame_after_show() {
        let timer = TimerFrames::default();
        let now = Instant::now();
        timer.start(Duration::from_millis(16));
        let (old, _) = timer.next(now).unwrap();
        timer.stop();
        assert_eq!(timer.next(now), None);
        timer.start(Duration::from_millis(8));
        let (new, _) = timer.next(now).unwrap();
        assert!(!timer.fire(old, now));
        assert!(timer.fire(new, now));
        assert!(!timer.fire(new, now));
    }
}
