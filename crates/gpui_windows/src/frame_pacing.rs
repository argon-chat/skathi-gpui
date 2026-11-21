//! vue-native fork: idle frame pacing.
//!
//! The vsync thread asks every window to paint on every refresh. That keeps a
//! frame clock running for embedders that do work in each frame, and costs a
//! UI-thread wakeup per refresh while nothing changes. An embedder that knows
//! it is idle calls [`pace_frames`] with the time it next needs a frame; until
//! then a refresh is delivered only when something asks for one:
//!
//! - gpui frame demand (a window became dirty, `on_next_frame`, a present
//!   request), through the window's frame waker;
//! - a message to a window (input, focus, resize and the like);
//! - a touchpad gesture or its inertia, which Direct Manipulation advances
//!   only from paints;
//! - [`wake_frames`], from any thread.
//!
//! A delivered refresh ends the pacing: the embedder decides again in that
//! frame. Without a [`pace_frames`] call nothing changes.
//!
//! While paced, the vsync thread does not wait on the compositor each refresh
//! either: it sleeps ([`wait_while_paced`]) until demand, the deadline, or a
//! new pacing, so an idle app has no thread waking at the display rate. While
//! present feedback is wanted it keeps waking, to read the frame statistics.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Nanoseconds after [`base`] until which refreshes wait for demand; 0 when
/// every refresh is delivered, [`UNTIL_DEMAND`] when only demand ends it.
static PACED_UNTIL: AtomicU64 = AtomicU64::new(0);
const UNTIL_DEMAND: u64 = u64::MAX;
/// The last stretch before a deadline is waited out refresh by refresh, so the
/// deadline is met on a vblank and not a timer tick late.
const VSYNC_MARGIN: Duration = Duration::from_millis(20);
/// Something asked for a frame since the last delivered refresh.
static DEMAND: AtomicBool = AtomicBool::new(false);
/// A window became dirty since the last delivered refresh: [`window_wants_frame`].
static WINDOW_DEMAND: AtomicBool = AtomicBool::new(false);
/// A touchpad gesture is running or coasting.
static GESTURE: AtomicBool = AtomicBool::new(false);

fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn stamp(at: Instant) -> u64 {
    // Never 0, which means "not paced", nor UNTIL_DEMAND.
    (at.saturating_duration_since(base()).as_nanos() as u64).clamp(1, UNTIL_DEMAND - 1)
}

/// Set when the paced state changed, so a sleeping vsync thread looks again.
fn kick_lock() -> &'static (Mutex<bool>, Condvar) {
    static KICK: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
    KICK.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

pub(crate) fn kick() {
    let (lock, cv) = kick_lock();
    *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
}

/// Deliver refreshes only on demand until `until`, or every refresh again
/// with `None`.
pub fn pace_frames(until: Option<Instant>) {
    PACED_UNTIL.store(until.map_or(0, stamp), Ordering::Release);
    kick();
}

/// [`pace_frames`] from inside a frame, before its draw: the windows dirtied so
/// far are what this frame is about to draw (an embedder that repaints from its
/// frame callback dirties them through the window's frame waker), so that does
/// not ask for the next refresh. A [`wake_frames`] still does, whenever it came:
/// it may announce work the frame has not seen. `None` waits for demand alone.
pub fn pace_frames_from_frame(until: Option<Instant>) {
    WINDOW_DEMAND.store(false, Ordering::Release);
    PACED_UNTIL.store(until.map_or(UNTIL_DEMAND, stamp), Ordering::Release);
    kick();
}

/// Ask for the next refresh to be delivered. Callable from any thread.
pub fn wake_frames() {
    DEMAND.store(true, Ordering::Release);
    kick();
}

/// A window's frame waker: as [`wake_frames`], but [`pace_frames_from_frame`] takes it back.
pub(crate) fn window_wants_frame() {
    WINDOW_DEMAND.store(true, Ordering::Release);
    kick();
}

fn demanded() -> bool {
    DEMAND.load(Ordering::Acquire) || WINDOW_DEMAND.load(Ordering::Acquire)
}

pub(crate) fn set_gesture_active(active: bool) {
    GESTURE.store(active, Ordering::Release);
    if active {
        kick();
    }
}

/// Called by the vsync thread before it waits for a refresh: sleeps while the
/// refresh would not be delivered anyway, until demand, a new pacing, or
/// [`VSYNC_MARGIN`] before the deadline.
pub(crate) fn wait_while_paced() {
    let (lock, cv) = kick_lock();
    let mut kicked = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        *kicked = false;
        let until = PACED_UNTIL.load(Ordering::Acquire);
        if until == 0
            || demanded()
            || GESTURE.load(Ordering::Acquire)
            || gpui::present_feedback::present_feedback_wanted()
        {
            return;
        }
        kicked = if until == UNTIL_DEMAND {
            cv.wait_while(kicked, |k| !*k)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            let left = Duration::from_nanos(until.saturating_sub(stamp(Instant::now())));
            if left <= VSYNC_MARGIN {
                return;
            }
            let (guard, timeout) = cv
                .wait_timeout_while(kicked, left - VSYNC_MARGIN, |k| !*k)
                .unwrap_or_else(|e| e.into_inner());
            if timeout.timed_out() {
                return;
            }
            guard
        };
    }
}

/// Whether a window message asks for a frame: everything but the paint and
/// timer traffic a window gets while idle.
pub(crate) fn message_wants_frame(msg: u32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{WM_GETOBJECT, WM_PAINT, WM_TIMER};
    !matches!(msg, WM_PAINT | WM_TIMER | WM_GETOBJECT)
}

/// Called by the vsync thread once per refresh: whether to deliver it.
pub(crate) fn take_due(now: Instant) -> bool {
    let demand = DEMAND.swap(false, Ordering::AcqRel) | WINDOW_DEMAND.swap(false, Ordering::AcqRel);
    let until = PACED_UNTIL.load(Ordering::Acquire);
    if until == 0 {
        return true;
    }
    if demand || GESTURE.load(Ordering::Acquire) || stamp(now) >= until {
        // Only this pacing: a newer one set meanwhile stands.
        let _ = PACED_UNTIL.compare_exchange(until, 0, Ordering::AcqRel, Ordering::Acquire);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // One test: the state is process-global.
    #[test]
    fn paced_refreshes_wait_for_demand_or_the_deadline() {
        let now = Instant::now();
        pace_frames(None);
        assert!(take_due(now), "unpaced, every refresh is delivered");

        pace_frames(Some(now + Duration::from_millis(30)));
        assert!(!take_due(now));
        assert!(!take_due(now + Duration::from_millis(10)));
        wake_frames();
        assert!(
            take_due(now + Duration::from_millis(11)),
            "demand is delivered"
        );
        assert!(
            take_due(now + Duration::from_millis(12)),
            "and ends the pacing"
        );

        pace_frames(Some(now + Duration::from_millis(30)));
        assert!(!take_due(now + Duration::from_millis(29)));
        assert!(
            take_due(now + Duration::from_millis(30)),
            "the deadline is delivered"
        );

        pace_frames(Some(now + Duration::from_secs(1)));
        set_gesture_active(true);
        assert!(take_due(now), "a gesture keeps refreshes coming");
        set_gesture_active(false);
        pace_frames(Some(now + Duration::from_secs(1)));
        assert!(!take_due(now));

        // Demand raised before the pacing began is still served.
        wake_frames();
        pace_frames(Some(now + Duration::from_secs(1)));
        assert!(take_due(now));

        // Paced from inside a frame, the windows it dirtied are what it draws.
        window_wants_frame();
        pace_frames_from_frame(Some(now + Duration::from_secs(1)));
        assert!(!take_due(now));
        window_wants_frame();
        assert!(take_due(now), "a window dirtied after it is served");

        // A wake from elsewhere may announce work the frame has not seen.
        wake_frames();
        pace_frames_from_frame(None);
        assert!(take_due(now));

        // Paced until demand: no deadline ends it.
        pace_frames_from_frame(None);
        assert!(!take_due(now + Duration::from_secs(3600)));
        wake_frames();
        assert!(take_due(now));
        pace_frames(None);

        // The vsync thread does not wait when unpaced, and sleeps until demand when paced.
        wait_while_paced();
        pace_frames_from_frame(None);
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            wait_while_paced();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        wake_frames();
        rx.recv_timeout(Duration::from_secs(5))
            .expect("demand wakes it");
        waiter.join().unwrap();
        assert!(take_due(now));
        pace_frames(None);
    }

    #[test]
    fn idle_traffic_does_not_ask_for_frames() {
        use windows::Win32::UI::WindowsAndMessaging::*;
        for msg in [WM_PAINT, WM_TIMER, WM_GETOBJECT] {
            assert!(!message_wants_frame(msg));
        }
        for msg in [
            WM_MOUSEMOVE,
            WM_KEYDOWN,
            WM_SIZE,
            WM_SETFOCUS,
            WM_MOUSEWHEEL,
        ] {
            assert!(message_wants_frame(msg));
        }
    }
}
