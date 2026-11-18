use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use gpui::FrameRequestSource;
use gpui_util::ResultExt;
use windows::Win32::{
    Foundation::HWND,
    Graphics::{
        Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo},
        Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput},
        Gdi::HMONITOR,
    },
    System::Performance::QueryPerformanceFrequency,
};

static QPC_TICKS_PER_SECOND: LazyLock<u64> = LazyLock::new(|| {
    let mut frequency = 0;
    // On systems that run Windows XP or later, the function will always succeed and
    // will thus never return zero.
    unsafe { QueryPerformanceFrequency(&mut frequency).unwrap() };
    frequency as u64
});

const VSYNC_INTERVAL_THRESHOLD: Duration = Duration::from_millis(1);
const DEFAULT_VSYNC_INTERVAL: Duration = Duration::from_micros(16_666); // ~60Hz

pub(crate) struct VSyncProvider {
    interval: Duration,
    vblank: Box<dyn FnMut() -> bool>,
    compositor: Box<dyn FnMut() -> bool>,
}

impl VSyncProvider {
    /// Waits on the vblank of the display `monitor` names, as Chromium's vsync thread does.
    /// `DwmFlush` returns once the compositor is done, which moves around in the refresh with
    /// its load, and after an idle stretch it can return anywhere in it.
    pub(crate) fn new(mut monitor: impl FnMut() -> HMONITOR + 'static) -> Self {
        let interval = get_dwm_interval()
            .context("Failed to get DWM interval")
            .log_err()
            .unwrap_or(DEFAULT_VSYNC_INTERVAL);
        let mut output = Output::default();
        Self {
            interval,
            vblank: Box::new(move || output.wait(monitor())),
            compositor: Box::new(|| unsafe { DwmFlush().is_ok() }),
        }
    }

    pub(crate) fn wait_for_vsync(&mut self) -> FrameRequestSource {
        // The wait functions return very early instead of waiting until vblank when the monitor
        // goes to sleep or is unplugged, and they can when a vblank was just about to come.
        // Under 1ms is not a vblank: the next one is, and if that returns early too, Sleep().
        if waited(&mut self.vblank) || waited(&mut self.vblank) || waited(&mut self.compositor) {
            return FrameRequestSource::NativeCallback;
        }
        log::trace!("VSyncProvider::wait_for_vsync() took less time than expected");
        std::thread::sleep(self.interval);
        FrameRequestSource::LocalSchedule
    }
}

fn waited(wait: &mut Box<dyn FnMut() -> bool>) -> bool {
    let start = Instant::now();
    wait() && start.elapsed() >= VSYNC_INTERVAL_THRESHOLD
}

/// The DXGI output of the last monitor waited on, looked up again when the monitor changes or a
/// wait fails (displays were reconfigured).
#[derive(Default)]
struct Output {
    monitor: HMONITOR,
    output: Option<IDXGIOutput>,
}

impl Output {
    fn wait(&mut self, monitor: HMONITOR) -> bool {
        if monitor != self.monitor || self.output.is_none() {
            self.monitor = monitor;
            self.output = find_output(monitor).log_err().flatten();
        }
        let Some(output) = &self.output else {
            return false;
        };
        let ok = unsafe { output.WaitForVBlank() }.is_ok();
        if !ok {
            self.output = None;
        }
        ok
    }
}

fn find_output(monitor: HMONITOR) -> Result<Option<IDXGIOutput>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    for adapter in (0..).map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok()) {
        for output in (0..).map_while(|i| unsafe { adapter.EnumOutputs(i) }.ok()) {
            if unsafe { output.GetDesc() }?.Monitor == monitor {
                return Ok(Some(output));
            }
        }
    }
    Ok(None)
}

fn get_dwm_interval() -> Result<Duration> {
    let mut timing_info = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing_info) }?;
    let interval = retrieve_duration(timing_info.qpcRefreshPeriod, *QPC_TICKS_PER_SECOND);
    // Check for interval values that are impossibly low. A 29 microsecond
    // interval was seen (from a qpcRefreshPeriod of 60).
    if interval < VSYNC_INTERVAL_THRESHOLD {
        Ok(retrieve_duration(
            timing_info.rateRefresh.uiDenominator as u64,
            timing_info.rateRefresh.uiNumerator as u64,
        ))
    } else {
        Ok(interval)
    }
}

#[inline]
fn retrieve_duration(counts: u64, ticks_per_second: u64) -> Duration {
    let ticks_per_microsecond = ticks_per_second / 1_000_000;
    Duration::from_micros(counts / ticks_per_microsecond)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;

    fn wait(ok: bool, calls: &Rc<Cell<u32>>) -> Box<dyn FnMut() -> bool> {
        let calls = calls.clone();
        Box::new(move || {
            calls.set(calls.get() + 1);
            std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
            ok
        })
    }

    fn early(calls: &Rc<Cell<u32>>) -> Box<dyn FnMut() -> bool> {
        let calls = calls.clone();
        Box::new(move || {
            calls.set(calls.get() + 1);
            true
        })
    }

    #[test]
    fn a_vblank_wait_reports_native_callback_without_the_compositor() {
        let (vblank, compositor) = (Rc::default(), Rc::default());
        let mut provider = VSyncProvider {
            interval: Duration::ZERO,
            vblank: wait(true, &vblank),
            compositor: wait(true, &compositor),
        };

        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
        assert_eq!((vblank.get(), compositor.get()), (1, 0));
    }

    #[test]
    fn a_vblank_wait_that_returns_at_once_waits_for_the_next_vblank() {
        let (vblank, compositor) = (Rc::<Cell<u32>>::default(), Rc::default());
        let calls = vblank.clone();
        let mut provider = VSyncProvider {
            interval: Duration::ZERO,
            vblank: Box::new(move || {
                calls.set(calls.get() + 1);
                if calls.get() > 1 {
                    std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
                }
                true
            }),
            compositor: wait(true, &compositor),
        };

        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
        assert_eq!((vblank.get(), compositor.get()), (2, 0));
    }

    #[test]
    fn without_a_vblank_the_compositor_is_waited_on() {
        let (vblank, compositor) = (Rc::default(), Rc::default());
        let mut provider = VSyncProvider {
            interval: Duration::ZERO,
            vblank: wait(false, &vblank),
            compositor: wait(true, &compositor),
        };

        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
        assert_eq!((vblank.get(), compositor.get()), (2, 1));
    }

    #[test]
    fn failed_or_early_waits_report_local_schedule() {
        let (vblank, compositor) = (Rc::default(), Rc::default());
        let mut provider = VSyncProvider {
            interval: Duration::ZERO,
            vblank: early(&vblank),
            compositor: wait(false, &compositor),
        };

        assert_eq!(provider.wait_for_vsync(), FrameRequestSource::LocalSchedule);
        assert_eq!((vblank.get(), compositor.get()), (2, 1));
    }
}
