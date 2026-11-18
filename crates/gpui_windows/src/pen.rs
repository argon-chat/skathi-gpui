//! Stylus input. A pen's `WM_POINTER*` messages are handled here rather than left to
//! `DefWindowProc`, which would turn them into mouse messages without pressure or tilt and run
//! the system's pen gestures (press-and-hold right click, flicks) on top of the stroke.

use std::cell::Cell;
use std::time::{Duration, Instant};

use gpui::{MouseButton, PenSample, Point, point, px};
use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::Gdi::ClientToScreen,
        System::SystemInformation::GetTickCount,
        UI::{Controls::*, Input::Pointer::*, WindowsAndMessaging::*},
    },
    core::BOOL,
};

pub(crate) const WM_TABLET_QUERYSYSTEMGESTURESTATUS: u32 = 0x02CC;

const TABLET_DISABLE_PRESSANDHOLD: u32 = 0x1;
const TABLET_DISABLE_PENTAPFEEDBACK: u32 = 0x8;
const TABLET_DISABLE_PENBARRELFEEDBACK: u32 = 0x10;
const TABLET_DISABLE_FLICKS: u32 = 0x10000;
const TABLET_DISABLE_FLICKFALLBACKKEYS: u32 = 0x100000;

/// The answer to `WM_TABLET_QUERYSYSTEMGESTURESTATUS`.
pub(crate) const TABLET_GESTURES_OFF: isize = (TABLET_DISABLE_PRESSANDHOLD
    | TABLET_DISABLE_PENTAPFEEDBACK
    | TABLET_DISABLE_PENBARRELFEEDBACK
    | TABLET_DISABLE_FLICKS
    | TABLET_DISABLE_FLICKFALLBACKKEYS) as isize;

/// Turns off the Windows Ink visuals (tap ripples, the barrel and press-and-hold indicators) for
/// `hwnd`: they draw over a stroke.
pub(crate) fn disable_feedback(hwnd: HWND) {
    let off = BOOL(0);
    for kind in [
        FEEDBACK_PEN_BARRELVISUALIZATION,
        FEEDBACK_PEN_TAP,
        FEEDBACK_PEN_DOUBLETAP,
        FEEDBACK_PEN_PRESSANDHOLD,
        FEEDBACK_PEN_RIGHTTAP,
        FEEDBACK_GESTURE_PRESSANDTAP,
    ] {
        // SAFETY: `off` outlives the call and its size is passed.
        unsafe {
            let _ = SetWindowFeedbackSetting(
                hwnd,
                kind,
                0,
                std::mem::size_of::<BOOL>() as u32,
                Some(&off as *const BOOL as *const _),
            );
        }
    }
}

/// The mouse button a pen contact pressed, from its down to its up.
#[derive(Default)]
pub(crate) struct PenTracker {
    pub(crate) down: Cell<Option<MouseButton>>,
}

/// One `POINTER_PEN_INFO`, in client device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PenReading {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) pointer_flags: u32,
    pub(crate) pen_flags: u32,
    pub(crate) pen_mask: u32,
    pub(crate) pressure: u32,
    pub(crate) rotation: u32,
    pub(crate) tilt_x: i32,
    pub(crate) tilt_y: i32,
    pub(crate) time: Option<Instant>,
}

impl PenReading {
    pub(crate) fn in_contact(&self) -> bool {
        self.pointer_flags & POINTER_FLAG_INCONTACT.0 != 0
    }

    pub(crate) fn barrel(&self) -> bool {
        self.pen_flags & PEN_FLAG_BARREL != 0
    }

    /// The eraser end is in range (inverted) or pressed.
    pub(crate) fn eraser(&self) -> bool {
        self.pen_flags & (PEN_FLAG_INVERTED | PEN_FLAG_ERASER) != 0
    }

    pub(crate) fn sample(&self, scale: f32) -> PenSample {
        let pressure = if self.pen_mask & PEN_MASK_PRESSURE != 0 {
            (self.pressure as f32 / 1024.0).clamp(0.0, 1.0)
        } else if self.in_contact() {
            0.5
        } else {
            0.0
        };
        let tilt = |mask: u32, v: i32| {
            if self.pen_mask & mask != 0 {
                (v as f32).clamp(-90.0, 90.0)
            } else {
                0.0
            }
        };
        PenSample {
            position: point(px(self.x / scale), px(self.y / scale)),
            pressure,
            tangential_pressure: 0.0,
            tilt_x: tilt(PEN_MASK_TILT_X, self.tilt_x),
            tilt_y: tilt(PEN_MASK_TILT_Y, self.tilt_y),
            twist: if self.pen_mask & PEN_MASK_ROTATION != 0 {
                (self.rotation % 360) as f32
            } else {
                0.0
            },
            width: 0.0,
            height: 0.0,
            time: self.time,
        }
    }
}

/// The clocks a reading's time is measured against, read together.
struct Now {
    instant: Instant,
    qpc: i64,
    tick: u32,
}

impl Now {
    fn read() -> Self {
        Self {
            instant: Instant::now(),
            qpc: crate::present_feedback::qpc(),
            // SAFETY: no arguments.
            tick: unsafe { GetTickCount() },
        }
    }

    /// When `info` was read: its `PerformanceCount` (QPC) when the device fills it in, else its
    /// `dwTime` (`GetTickCount` milliseconds).
    fn of(&self, info: &POINTER_INFO) -> Option<Instant> {
        let age = if info.PerformanceCount != 0 {
            Duration::from_nanos(crate::present_feedback::qpc_ns(
                self.qpc - info.PerformanceCount as i64,
            ))
        } else if info.dwTime != 0 {
            // A time after `tick` wraps past `i32::MAX`.
            let ms = self.tick.wrapping_sub(info.dwTime);
            Duration::from_millis(if ms > i32::MAX as u32 { 0 } else { ms.into() })
        } else {
            return None;
        };
        self.instant.checked_sub(age)
    }
}

/// What one pen message means for gpui, given the button the pen's contact holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PenStep {
    Down(MouseButton),
    Move(Option<MouseButton>),
    Up(MouseButton),
    Exited,
}

pub(crate) fn steps(
    msg: u32,
    contact: bool,
    barrel: bool,
    down: Option<MouseButton>,
) -> smallvec::SmallVec<[PenStep; 2]> {
    let pressed = if barrel {
        MouseButton::Right
    } else {
        MouseButton::Left
    };
    let mut out = smallvec::SmallVec::new();
    match msg {
        WM_POINTERDOWN => {
            if let Some(held) = down {
                out.push(PenStep::Up(held));
            }
            out.push(PenStep::Down(pressed));
        }
        WM_POINTERUP => match down {
            Some(held) => out.push(PenStep::Up(held)),
            None => out.push(PenStep::Move(None)),
        },
        WM_POINTERLEAVE | WM_POINTERCAPTURECHANGED => {
            if let Some(held) = down {
                out.push(PenStep::Up(held));
            }
            out.push(PenStep::Exited);
        }
        _ => match (contact, down) {
            (false, Some(held)) => {
                out.push(PenStep::Up(held));
                out.push(PenStep::Move(None));
            }
            (true, None) => out.push(PenStep::Down(pressed)),
            (_, held) => out.push(PenStep::Move(held)),
        },
    }
    out
}

/// The pen location with the precision the digitizer has: `ptHimetricLocation` mapped through
/// the device's rectangles, unless that disagrees with the system's own pixel location (a
/// rotated display, a tablet mapped to part of a screen).
pub(crate) fn subpixel(pixel: POINT, himetric: POINT, rects: Option<(RECT, RECT)>) -> (f32, f32) {
    let whole = (pixel.x as f32, pixel.y as f32);
    let Some((device, display)) = rects else {
        return whole;
    };
    let (dw, dh) = (
        (device.right - device.left) as f32,
        (device.bottom - device.top) as f32,
    );
    if dw <= 0.0 || dh <= 0.0 {
        return whole;
    }
    let x = display.left as f32
        + (himetric.x - device.left) as f32 * (display.right - display.left) as f32 / dw;
    let y = display.top as f32
        + (himetric.y - device.top) as f32 * (display.bottom - display.top) as f32 / dh;
    if (x - whole.0).abs() > 1.5 || (y - whole.1).abs() > 1.5 {
        return whole;
    }
    (x, y)
}

/// The pen's readings since its previous message, oldest first, in client device pixels.
/// `None` when the pointer is not a pen.
pub(crate) fn read(hwnd: HWND, pointer_id: u32) -> Option<Vec<PenReading>> {
    let mut kind = POINTER_INPUT_TYPE::default();
    // SAFETY: plain out-parameters.
    unsafe { GetPointerType(pointer_id, &mut kind) }.ok()?;
    if kind != PT_PEN {
        return None;
    }
    let mut latest = POINTER_PEN_INFO::default();
    // SAFETY: plain out-parameter.
    unsafe { GetPointerPenInfo(pointer_id, &mut latest) }.ok()?;
    let mut infos = vec![latest];
    let history = latest.pointerInfo.historyCount;
    if history > 1 {
        let mut count = history;
        let mut all = vec![POINTER_PEN_INFO::default(); history as usize];
        // SAFETY: `all` holds `count` entries.
        if unsafe { GetPointerPenInfoHistory(pointer_id, &mut count, Some(all.as_mut_ptr())) }
            .is_ok()
        {
            all.truncate(count as usize);
            if !all.is_empty() {
                infos = all;
            }
        }
    }
    let mut origin = POINT::default();
    // SAFETY: plain in/out parameter.
    let _ = unsafe { ClientToScreen(hwnd, &mut origin) };
    let mut device = RECT::default();
    let mut display = RECT::default();
    // SAFETY: plain out-parameters.
    let rects = unsafe {
        GetPointerDeviceRects(latest.pointerInfo.sourceDevice, &mut device, &mut display)
    }
    .ok()
    .map(|_| (device, display));
    let now = Now::read();
    // The history is newest first.
    Some(
        infos
            .iter()
            .rev()
            .map(|info| {
                let p = &info.pointerInfo;
                let (x, y) = subpixel(p.ptPixelLocation, p.ptHimetricLocation, rects);
                PenReading {
                    x: x - origin.x as f32,
                    y: y - origin.y as f32,
                    pointer_flags: p.pointerFlags.0,
                    pen_flags: info.penFlags,
                    pen_mask: info.penMask,
                    pressure: info.pressure,
                    rotation: info.rotation,
                    tilt_x: info.tiltX,
                    tilt_y: info.tiltY,
                    time: now.of(p),
                }
            })
            .collect(),
    )
}

pub(crate) fn position(reading: &PenReading, scale: f32) -> Point<gpui::Pixels> {
    point(px(reading.x / scale), px(reading.y / scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(flags: u32, pen_flags: u32, mask: u32) -> PenReading {
        PenReading {
            x: 300.0,
            y: 150.0,
            pointer_flags: flags,
            pen_flags,
            pen_mask: mask,
            pressure: 512,
            rotation: 370,
            tilt_x: 30,
            tilt_y: -95,
            time: None,
        }
    }

    #[test]
    fn readings_become_logical_samples() {
        let contact = POINTER_FLAG_INCONTACT.0 | POINTER_FLAG_INRANGE.0;
        let all = PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
        let s = reading(contact, 0, all).sample(1.5);
        assert_eq!(s.position, point(px(200.0), px(100.0)));
        assert_eq!(s.pressure, 0.5);
        assert_eq!((s.tilt_x, s.tilt_y, s.twist), (30.0, -90.0, 10.0));

        let bare = reading(contact, 0, 0).sample(1.0);
        assert_eq!(
            (bare.pressure, bare.tilt_x, bare.twist),
            (0.5, 0.0, 0.0),
            "no pressure data: 0.5 in contact"
        );
        let hover = reading(POINTER_FLAG_INRANGE.0, 0, 0).sample(1.0);
        assert_eq!(hover.pressure, 0.0);
        let mut hard = reading(contact, 0, PEN_MASK_PRESSURE);
        hard.pressure = 4096;
        assert_eq!(hard.sample(1.0).pressure, 1.0);
    }

    #[test]
    fn eraser_and_barrel_flags() {
        assert!(reading(0, PEN_FLAG_INVERTED, 0).eraser());
        assert!(reading(0, PEN_FLAG_ERASER, 0).eraser());
        assert!(!reading(0, PEN_FLAG_BARREL, 0).eraser());
        assert!(reading(0, PEN_FLAG_BARREL, 0).barrel());
    }

    #[test]
    fn messages_map_to_a_press_its_moves_and_its_release() {
        use MouseButton::*;
        use PenStep::*;
        let s = |msg, contact, barrel, down| steps(msg, contact, barrel, down).to_vec();
        assert_eq!(
            s(WM_POINTERUPDATE, false, false, None),
            [Move(None)],
            "hover"
        );
        assert_eq!(s(WM_POINTERDOWN, true, false, None), [Down(Left)]);
        assert_eq!(s(WM_POINTERDOWN, true, true, None), [Down(Right)], "barrel");
        assert_eq!(
            s(WM_POINTERUPDATE, true, true, Some(Left)),
            [Move(Some(Left))]
        );
        assert_eq!(s(WM_POINTERUP, false, false, Some(Right)), [Up(Right)]);
        assert_eq!(s(WM_POINTERUP, false, false, None), [Move(None)]);
        assert_eq!(
            s(WM_POINTERUPDATE, false, false, Some(Left)),
            [Up(Left), Move(None)],
            "a lost up"
        );
        assert_eq!(
            s(WM_POINTERUPDATE, true, false, None),
            [Down(Left)],
            "a lost down"
        );
        assert_eq!(s(WM_POINTERLEAVE, false, false, None), [Exited]);
        assert_eq!(
            s(WM_POINTERCAPTURECHANGED, true, false, Some(Left)),
            [Up(Left), Exited]
        );
    }

    #[test]
    fn himetric_refines_the_pixel_location() {
        let device = RECT {
            left: 0,
            top: 0,
            right: 20000,
            bottom: 10000,
        };
        let display = RECT {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 1000,
        };
        let at = |x, y| POINT { x, y };
        assert_eq!(
            subpixel(at(100, 50), at(1004, 507), Some((device, display))),
            (100.4, 50.7)
        );
        assert_eq!(
            subpixel(at(100, 50), at(5000, 507), Some((device, display))),
            (100.0, 50.0),
            "a mapping that disagrees falls back"
        );
        assert_eq!(subpixel(at(7, 9), at(0, 0), None), (7.0, 9.0));
    }
}
