//! vue-native fork: when each window present reached the screen, for an embedder that times its
//! own frames against the display.
//!
//! A platform that reports it calls [`PresentListener::queued`] on the presenting thread for each
//! present while feedback is wanted, then [`PresentListener::resolved`] once for that present, in
//! present order per window. Times are the platform's monotonic clock in nanoseconds: QPC on
//! Windows, `mach_absolute_time` on macOS.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// What became of a window present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    /// First shown on a refresh.
    Displayed {
        /// When that refresh began.
        at_ns: u64,
        /// Its distance from the refresh before; the nominal period where that was not seen.
        refresh_ns: u64,
        /// False when `at_ns` was estimated.
        exact: bool,
    },
    /// Replaced by a later present, or dropped with its swap chain, before any refresh showed it.
    Discarded,
}

/// Hears window presents; see the module docs.
pub trait PresentListener: Send + Sync {
    /// On the presenting thread, after the present was queued and before it can be resolved.
    /// `window` is the native handle: the HWND on Windows, the view's `CAMetalLayer` on macOS.
    fn queued(&self, window: u64, present: u64);
    /// Once per queued present, in present order per window.
    fn resolved(&self, window: u64, present: u64, outcome: PresentOutcome);
    /// On Windows, once per refresh while feedback is wanted, after that refresh's
    /// [`Self::resolved`] calls: from the vsync thread, after it asked the windows to paint, or
    /// with [`set_feedback_after_composition`] from a thread of its own.
    fn refresh(&self) {}
}

static LISTENER: OnceLock<Box<dyn PresentListener>> = OnceLock::new();
static WANTED: AtomicBool = AtomicBool::new(false);
static AFTER_COMPOSITION: AtomicBool = AtomicBool::new(false);

/// On Windows, reads each refresh's feedback once the compositor has finished that refresh: every
/// outcome is exact, and no present misses its refresh to another application's update, but every
/// present is shown a refresh later. Off by default.
pub fn set_feedback_after_composition(on: bool) {
    AFTER_COMPOSITION.store(on, Ordering::Release);
}

/// Whether [`set_feedback_after_composition`] is on.
pub fn feedback_after_composition() -> bool {
    AFTER_COMPOSITION.load(Ordering::Acquire)
}
static WAKER: OnceLock<fn()> = OnceLock::new();

/// Installs the listener, once per process.
pub fn set_present_listener(listener: Box<dyn PresentListener>) -> Result<(), &'static str> {
    LISTENER
        .set(listener)
        .map_err(|_| "a present listener is already installed")
}

/// The installed listener.
pub fn present_listener() -> Option<&'static dyn PresentListener> {
    LISTENER.get().map(|l| &**l)
}

/// Whether presents are tracked and, on Windows, the vsync thread polls every refresh. Keep it
/// off while nothing waits for an outcome, so an idle window costs nothing.
pub fn want_present_feedback(on: bool) {
    if WANTED.swap(on, Ordering::AcqRel) != on && on {
        if let Some(wake) = WAKER.get() {
            wake();
        }
    }
}

/// Whether a listener is installed and wants feedback.
pub fn present_feedback_wanted() -> bool {
    WANTED.load(Ordering::Acquire) && LISTENER.get().is_some()
}

/// For a platform: called when feedback becomes wanted, to wake a sleeping vsync thread.
pub fn set_present_feedback_waker(wake: fn()) {
    let _ = WAKER.set(wake);
}

/// The refresh grid of the display the platform's vsync thread waits on, measured from its waits:
/// a vblank and the period, in the platform's monotonic clock in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VBlankClock {
    /// A vblank, not necessarily the latest.
    pub vblank_ns: u64,
    /// The measured distance between vblanks.
    pub period_ns: u64,
}

static CLOCK: Mutex<Option<VBlankClock>> = Mutex::new(None);

/// For a platform: the vsync thread's latest measure of its display's vblanks.
pub fn set_vblank_clock(clock: Option<VBlankClock>) {
    *CLOCK.lock().unwrap_or_else(|e| e.into_inner()) = clock;
}

/// The vsync thread's display's vblanks, where the platform measures them (Windows).
pub fn vblank_clock() -> Option<VBlankClock> {
    *CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}
