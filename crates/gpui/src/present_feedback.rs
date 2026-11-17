//! vue-native fork: when each window present reached the screen, for an embedder that times its
//! own frames against the display.
//!
//! A platform that reports it calls [`PresentListener::queued`] on the presenting thread for each
//! present while feedback is wanted, then [`PresentListener::resolved`] once for that present, in
//! present order per window. Times are the platform's monotonic clock in nanoseconds: QPC on
//! Windows, `mach_absolute_time` on macOS.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// On Windows, from the vsync thread once per refresh while feedback is wanted, after that
    /// refresh's [`Self::resolved`] calls.
    fn refresh(&self) {}
}

static LISTENER: OnceLock<Box<dyn PresentListener>> = OnceLock::new();
static WANTED: AtomicBool = AtomicBool::new(false);
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
