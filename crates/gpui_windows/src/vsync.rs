use std::{
    collections::VecDeque,
    sync::LazyLock,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use gpui::FrameRequestSource;
use gpui::present_feedback::{VBlankClock, set_vblank_clock};
use gpui_util::ResultExt;
use windows::Win32::{
    Foundation::HWND,
    Graphics::{
        Dwm::{DWM_TIMING_INFO, DwmFlush, DwmGetCompositionTimingInfo},
        Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput},
        Gdi::{
            DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HMONITOR,
            MONITORINFOEXW,
        },
    },
    System::Performance::QueryPerformanceFrequency,
};

use crate::present_feedback::{qpc, qpc_ns};

static QPC_TICKS_PER_SECOND: LazyLock<u64> = LazyLock::new(|| {
    let mut frequency = 0;
    // On systems that run Windows XP or later, the function will always succeed and
    // will thus never return zero.
    unsafe { QueryPerformanceFrequency(&mut frequency).unwrap() };
    frequency as u64
});

const VSYNC_INTERVAL_THRESHOLD: Duration = Duration::from_millis(1);
const DEFAULT_VSYNC_INTERVAL: Duration = Duration::from_micros(16_666); // ~60Hz

/// A display a vblank wait returned on: its handle, and its nominal refresh period in QPC ticks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Display {
    id: isize,
    nominal: f64,
}

pub(crate) struct VSyncProvider {
    interval: Duration,
    vblank: Box<dyn FnMut() -> Option<Display>>,
    compositor: Box<dyn FnMut() -> bool>,
    grid: VBlankGrid,
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
            grid: VBlankGrid::default(),
        }
    }

    pub(crate) fn wait_for_vsync(&mut self) -> FrameRequestSource {
        // The wait functions return very early instead of waiting until vblank when the monitor
        // goes to sleep or is unplugged, and they can when a vblank was just about to come.
        // Under 1ms is not a vblank: the next one is, and if that returns early too, Sleep().
        if let Some(display) = waited(&mut self.vblank).or_else(|| waited(&mut self.vblank)) {
            self.measured(display, qpc());
            return FrameRequestSource::NativeCallback;
        }
        let start = Instant::now();
        if (self.compositor)() && start.elapsed() >= VSYNC_INTERVAL_THRESHOLD {
            return FrameRequestSource::NativeCallback;
        }
        log::trace!("VSyncProvider::wait_for_vsync() took less time than expected");
        std::thread::sleep(self.interval);
        FrameRequestSource::LocalSchedule
    }

    fn measured(&mut self, display: Display, at: i64) {
        let Some((vblank, period)) = self.grid.waited(display, at) else {
            return;
        };
        set_vblank_clock(Some(VBlankClock {
            vblank_ns: qpc_ns(vblank),
            period_ns: (period * 1e9 / *QPC_TICKS_PER_SECOND as f64) as u64,
        }));
    }
}

fn waited<T>(wait: &mut Box<dyn FnMut() -> Option<T>>) -> Option<T> {
    let start = Instant::now();
    wait().filter(|_| start.elapsed() >= VSYNC_INTERVAL_THRESHOLD)
}

/// Waits kept to measure a display's refreshes by, and how many refreshes back they may go.
const KEPT_WAITS: usize = 64;
const KEPT_REFRESHES: f64 = 480.0;

/// A display's refresh grid, from when vblank waits on it return. A wait returns some tens of
/// microseconds after its vblank, later when the thread is slow to run: the period is fitted
/// through the recent waits, then through those that woke soonest, and the earliest wake marks the
/// grid. The nominal period only tells how many refreshes lie between two waits.
#[derive(Default)]
pub(crate) struct VBlankGrid {
    display: Option<Display>,
    waits: VecDeque<i64>,
    period: f64,
    vblank: Option<i64>,
    /// Waits in a row that came well after the grid's vblank: a slow wake, or the grid moved.
    late: Vec<i64>,
}

impl VBlankGrid {
    /// A wait on `display` returned at `at`, in QPC ticks: a vblank and the period, in ticks.
    pub(crate) fn waited(&mut self, display: Display, at: i64) -> Option<(i64, f64)> {
        let nominal = display.nominal.max(1.0);
        let same = self
            .display
            .is_some_and(|d| d.id == display.id && (d.nominal - nominal).abs() <= nominal * 0.01);
        if !same {
            self.display = Some(display);
            self.restart(nominal);
        }
        while self
            .waits
            .front()
            .is_some_and(|&w| (at - w) as f64 > KEPT_REFRESHES * self.period)
        {
            self.waits.pop_front();
        }
        // A wait returns after its vblank, never before: one before the grid's, or several well
        // after it, mean the display's timing moved under the same mode.
        if let Some(vblank) = self.vblank {
            let d = (at - vblank) as f64;
            let off = d - (d / self.period).round() * self.period;
            if off < -self.period / 8.0 {
                self.restart(nominal);
            } else if off > self.period / 8.0 {
                self.late.push(at);
                if self.late.len() < 3 {
                    return Some((vblank, self.period));
                }
                let late = std::mem::take(&mut self.late);
                self.restart(nominal);
                self.waits.extend(&late[..late.len() - 1]);
            } else {
                self.late.clear();
            }
        }
        self.waits.push_back(at);
        if self.waits.len() > KEPT_WAITS {
            self.waits.pop_front();
        }
        let points: Vec<(f64, f64)> = self
            .waits
            .iter()
            .map(|&w| {
                let d = (w - at) as f64;
                ((d / self.period).round(), d)
            })
            .collect();
        if let Some(period) = slope(&points) {
            let floor = offsets(&points, period).fold(f64::INFINITY, f64::min);
            let soonest: Vec<(f64, f64)> = points
                .iter()
                .copied()
                .filter(|&(n, d)| d - n * period - floor < period * 0.02)
                .collect();
            self.period = slope(&soonest).unwrap_or(period);
        }
        let early = offsets(&points, self.period).fold(f64::INFINITY, f64::min);
        let vblank = at + early.round() as i64;
        self.vblank = Some(vblank);
        Some((vblank, self.period))
    }

    fn restart(&mut self, nominal: f64) {
        self.waits.clear();
        self.period = nominal;
        self.vblank = None;
        self.late.clear();
    }
}

/// Where each wait falls against a grid of `period` through the newest.
fn offsets(points: &[(f64, f64)], period: f64) -> impl Iterator<Item = f64> + '_ {
    points.iter().map(move |&(n, d)| d - n * period)
}

/// The least-squares slope of `points`, given they span a few refreshes.
fn slope(points: &[(f64, f64)]) -> Option<f64> {
    let count = points.len() as f64;
    let (sn, sd) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), &(n, d)| (a + n, b + d));
    let (mn, md) = (sn / count, sd / count);
    let (snd, snn) = points.iter().fold((0.0, 0.0), |(a, b), &(n, d)| {
        (a + (n - mn) * (d - md), b + (n - mn) * (n - mn))
    });
    (points.len() >= 3 && snn >= 2.0).then(|| snd / snn)
}

/// The DXGI output of the last monitor waited on, looked up again when the monitor changes or a
/// wait fails (displays were reconfigured).
#[derive(Default)]
struct Output {
    monitor: HMONITOR,
    output: Option<IDXGIOutput>,
    nominal: Option<(f64, Instant)>,
}

impl Output {
    fn wait(&mut self, monitor: HMONITOR) -> Option<Display> {
        if monitor != self.monitor || self.output.is_none() {
            self.monitor = monitor;
            self.output = find_output(monitor).log_err().flatten();
            self.nominal = None;
        }
        let output = self.output.as_ref()?;
        if unsafe { output.WaitForVBlank() }.is_err() {
            self.output = None;
            return None;
        }
        // The mode can change under the same monitor; reading it is cheap but not free.
        if self
            .nominal
            .is_none_or(|(_, read)| read.elapsed() >= Duration::from_secs(1))
        {
            let hz = refresh_hz(monitor).unwrap_or(0);
            let nominal = if hz > 1 {
                *QPC_TICKS_PER_SECOND as f64 / f64::from(hz)
            } else {
                get_dwm_interval()
                    .map_or(DEFAULT_VSYNC_INTERVAL, |d| d)
                    .as_secs_f64()
                    * *QPC_TICKS_PER_SECOND as f64
            };
            self.nominal = Some((nominal, Instant::now()));
        }
        Some(Display {
            id: monitor.0 as isize,
            nominal: self.nominal?.0,
        })
    }
}

/// The monitor's current mode's refresh rate, in whole hertz.
fn refresh_hz(monitor: HMONITOR) -> Option<u32> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: a sized struct for a live monitor handle.
    unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }
        .as_bool()
        .then_some(())?;
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: the device name GetMonitorInfoW filled in, and a sized struct.
    unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    }
    .as_bool()
    .then_some(mode.dmDisplayFrequency)
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

    const DISPLAY: Display = Display {
        id: 1,
        nominal: 1000.0,
    };

    fn provider(
        vblank: Box<dyn FnMut() -> Option<Display>>,
        compositor: Box<dyn FnMut() -> bool>,
    ) -> VSyncProvider {
        VSyncProvider {
            interval: Duration::ZERO,
            vblank,
            compositor,
            grid: VBlankGrid::default(),
        }
    }

    fn vblank(ok: bool, calls: &Rc<Cell<u32>>) -> Box<dyn FnMut() -> Option<Display>> {
        let calls = calls.clone();
        Box::new(move || {
            calls.set(calls.get() + 1);
            std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
            ok.then_some(DISPLAY)
        })
    }

    fn compositor(ok: bool, calls: &Rc<Cell<u32>>) -> Box<dyn FnMut() -> bool> {
        let calls = calls.clone();
        Box::new(move || {
            calls.set(calls.get() + 1);
            std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
            ok
        })
    }

    #[test]
    fn a_vblank_wait_reports_native_callback_without_the_compositor() {
        let (v, c) = (Rc::default(), Rc::default());
        let mut provider = provider(vblank(true, &v), compositor(true, &c));
        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
        assert_eq!((v.get(), c.get()), (1, 0));
    }

    #[test]
    fn a_vblank_wait_that_returns_at_once_waits_for_the_next_vblank() {
        let (v, c) = (Rc::<Cell<u32>>::default(), Rc::default());
        let calls = v.clone();
        let mut provider = provider(
            Box::new(move || {
                calls.set(calls.get() + 1);
                if calls.get() > 1 {
                    std::thread::sleep(VSYNC_INTERVAL_THRESHOLD * 2);
                }
                Some(DISPLAY)
            }),
            compositor(true, &c),
        );
        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
        assert_eq!((v.get(), c.get()), (2, 0));
    }

    #[test]
    fn without_a_vblank_the_compositor_is_waited_on() {
        let (v, c) = (Rc::default(), Rc::default());
        let mut provider = provider(vblank(false, &v), compositor(true, &c));
        assert_eq!(
            provider.wait_for_vsync(),
            FrameRequestSource::NativeCallback
        );
        assert_eq!((v.get(), c.get()), (2, 1));
    }

    #[test]
    fn failed_or_early_waits_report_local_schedule() {
        let (v, c) = (Rc::<Cell<u32>>::default(), Rc::default());
        let calls = v.clone();
        let mut provider = provider(
            Box::new(move || {
                calls.set(calls.get() + 1);
                Some(DISPLAY)
            }),
            compositor(false, &c),
        );
        assert_eq!(provider.wait_for_vsync(), FrameRequestSource::LocalSchedule);
        assert_eq!((v.get(), c.get()), (2, 1));
    }

    /// Waits on a display refreshing every `period` ticks from `phase`, each returning `late`
    /// ticks after its vblank.
    fn waits(period: f64, phase: f64, refreshes: &[i64], late: &[i64]) -> Vec<i64> {
        refreshes
            .iter()
            .zip(late.iter().cycle())
            .map(|(&n, &l)| (phase + n as f64 * period).round() as i64 + l)
            .collect()
    }

    #[test]
    fn the_grid_is_the_earliest_wake_and_the_measured_period() {
        // 165 Hz nominally, 164.8 Hz really, in 10 MHz ticks; waits every refresh while frames
        // are pending, and now and then after an idle stretch. Wakes are 30-90 us late, one 2 ms.
        let display = Display {
            id: 1,
            nominal: 1e7 / 165.0,
        };
        let (period, phase) = (1e7 / 164.8, 2500.0);
        let refreshes: Vec<i64> = (0..8)
            .chain([15, 22, 29])
            .chain(30..40)
            .chain([47, 54])
            .collect();
        let late = [400, 350, 900, 420, 20_000, 300, 610, 380, 450];
        let mut grid = VBlankGrid::default();
        let mut last = None;
        for at in waits(period, phase, &refreshes, &late) {
            last = grid.waited(display, at);
        }
        let (vblank, measured) = last.unwrap();
        assert!(
            (measured - period).abs() < period * 3e-4,
            "period {measured}"
        );
        let off = (vblank as f64 - phase) / period;
        let after = (off - off.round()) * period;
        assert!(
            (after - 300.0).abs() < 60.0,
            "{after} ticks after the vblank"
        );
    }

    #[test]
    fn a_new_refresh_rate_or_display_starts_the_grid_over() {
        let mut grid = VBlankGrid::default();
        for at in waits(1000.0, 0.0, &[0, 1, 2, 3], &[20]) {
            grid.waited(DISPLAY, at);
        }
        // The mode changed to twice the period, and the grid moved.
        let slower = Display {
            nominal: 2000.0,
            ..DISPLAY
        };
        let mut last = None;
        for at in waits(2000.0, 4700.0, &[0, 1, 2], &[20]) {
            last = grid.waited(slower, at);
        }
        let (vblank, period) = last.unwrap();
        assert!((period - 2000.0).abs() < 1.0);
        assert_eq!((vblank - 4720) % 2000, 0);
        // Same mode, but the vblanks moved later: once that holds, only the new ones count.
        let mut last = None;
        for at in waits(2000.0, 15_300.0, &[0, 1, 2], &[20]) {
            last = grid.waited(slower, at);
        }
        assert_eq!((last.unwrap().0 - 15_320) % 2000, 0);
        // And earlier: at once.
        let last = grid.waited(slower, 15_320 + 3 * 2000 + 1500);
        assert_eq!((last.unwrap().0 - 22_820) % 2000, 0);
    }
}
