use super::{FrameSource, MacWindowState};
use futures::{StreamExt, channel::mpsc};
use gpui::{FrameRequestSource, PlatformFrameSignal, RequestFrameOptions, SurfaceFrameWaker};
use parking_lot::Mutex;
use std::{
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

mod cadence;
use cadence::{Cadence, Pacing};

pub(super) fn listen(window: &Rc<Mutex<MacWindowState>>) -> SurfaceFrameWaker {
    let cadence = Arc::new(Mutex::new(Cadence::default()));
    let (sender, mut receiver) = mpsc::unbounded();
    let executor = window.lock().foreground_executor.clone();
    let window = Rc::downgrade(window);
    let incoming = cadence.clone();
    executor
        .spawn(async move {
            while receiver.next().await.is_some() {
                let pacing = incoming.lock().pending.take();
                let Some(window) = window.upgrade() else {
                    break;
                };
                if let Some(pacing) = pacing {
                    draw_ready(&window, pacing);
                }
            }
        })
        .detach();
    Arc::new(move || {
        if cadence.lock().ready(Instant::now()) {
            let _ = sender.unbounded_send(());
        }
    })
}

fn draw_ready(window: &Rc<Mutex<MacWindowState>>, pacing: Pacing) {
    let mut state = window.lock();
    if state.closed.load(Ordering::Acquire) || !state.draws() {
        return;
    }
    if let Some(invalidator) = &state.surface_invalidator {
        invalidator();
    }
    log::trace!(target: "gpui_macos::surface_pacing", "{} window={} ready {pacing:?}",
        host_ns(), state.handle.window_id().as_u64());
    if pacing == Pacing::DisplayLink {
        if let FrameSource::Metal(frames) = &state.frame_source {
            frames.wake();
        }
        return;
    }
    let Some(mut callback) = state.request_frame_callback.take() else {
        return;
    };
    // CAMetalDisplayLink owns the layer's drawables until invalidated.
    let attached =
        matches!(&state.frame_source, FrameSource::Metal(frames) if frames.is_attached());
    if attached {
        state.stop_display_link();
    }
    drop(state);
    callback(RequestFrameOptions {
        require_presentation: true,
        signal_at: PlatformFrameSignal::capture(scheduler::Instant::now),
        signal_source: FrameRequestSource::LocalSchedule,
        ..Default::default()
    });
    let mut state = window.lock();
    state.request_frame_callback = Some(callback);
    if attached && !state.closed.load(Ordering::Acquire) {
        state.start_display_link(window);
    }
}

pub(super) fn host_ns() -> u64 {
    let ticks = objc2_core_video::CVGetCurrentHostTime();
    let hz = objc2_core_video::CVGetHostClockFrequency();
    (ticks as f64 * 1e9 / hz) as u64
}

