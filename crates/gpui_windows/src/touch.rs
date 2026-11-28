//! Touch contacts. A finger's `WM_POINTER*` messages are handled here rather than left to
//! `DefWindowProc`, which would turn the first contact into mouse messages and run the system's
//! touch gestures over it.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Pixels, Point, Size, TouchEvent, TouchId, TouchPhase, point, px, size};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::ClientToScreen,
    UI::{Input::Pointer::*, WindowsAndMessaging::*},
};

/// Windows reuses pointer ids; a [`TouchId`] is never reused.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type Reported = (Point<Pixels>, Option<Size<Pixels>>, Option<f32>);

struct Tracked {
    pointer_id: u32,
    id: TouchId,
    last: Reported,
    screen: (f32, f32),
}

/// A window move or resize a finger drives: where it started, and the window then.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Drag {
    pub(crate) pointer_id: u32,
    pub(crate) from: (f32, f32),
    pub(crate) rect: RECT,
    pub(crate) resize: Option<u32>,
}

/// The contacts down in one window, any number of them.
#[derive(Default)]
pub(crate) struct TouchTracker {
    down: RefCell<Vec<Tracked>>,
    /// Ends the window could not deliver (its input callback was busy), replayed as cancels.
    undelivered: RefCell<Vec<TouchEvent>>,
    drag: Cell<Option<Drag>>,
}

impl TouchTracker {
    pub(crate) fn find(&self, pointer_id: u32) -> Option<(TouchId, Point<Pixels>)> {
        self.down
            .borrow()
            .iter()
            .find(|t| t.pointer_id == pointer_id)
            .map(|t| (t.id, t.last.0))
    }

    pub(crate) fn start(&self, pointer_id: u32, event: &TouchEvent, screen: (f32, f32)) -> TouchId {
        let id = TouchId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let mut down = self.down.borrow_mut();
        down.retain(|t| t.pointer_id != pointer_id);
        down.push(Tracked {
            pointer_id,
            id,
            last: (event.position, event.size, event.force),
            screen,
        });
        id
    }

    /// Records a reading; false when it repeats the last one (a finger held still).
    pub(crate) fn moved(&self, pointer_id: u32, event: &TouchEvent, screen: (f32, f32)) -> bool {
        let mut down = self.down.borrow_mut();
        let Some(t) = down.iter_mut().find(|t| t.pointer_id == pointer_id) else {
            return false;
        };
        let now = (event.position, event.size, event.force);
        t.screen = screen;
        std::mem::replace(&mut t.last, now) != now
    }

    pub(crate) fn end(&self, pointer_id: u32) {
        self.down
            .borrow_mut()
            .retain(|t| t.pointer_id != pointer_id);
    }

    /// The oldest contact down, with its screen position: the one a window drag follows.
    pub(crate) fn first(&self) -> Option<(u32, TouchId, Point<Pixels>, (f32, f32))> {
        self.down
            .borrow()
            .first()
            .map(|t| (t.pointer_id, t.id, t.last.0, t.screen))
    }

    /// What to hand the window now for `event`: ends it could not take earlier, as cancels,
    /// then `event`. With no window to take them (`deliverable` false), an end waits.
    pub(crate) fn outbox(&self, event: TouchEvent, deliverable: bool) -> Vec<TouchEvent> {
        let mut undelivered = self.undelivered.borrow_mut();
        if !deliverable {
            if matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                undelivered.push(TouchEvent {
                    phase: TouchPhase::Cancelled,
                    ..event
                });
            }
            return Vec::new();
        }
        let mut out: Vec<TouchEvent> = undelivered.drain(..).collect();
        out.push(event);
        out
    }

    pub(crate) fn drag(&self) -> Option<Drag> {
        self.drag.get()
    }

    pub(crate) fn set_drag(&self, drag: Option<Drag>) {
        self.drag.set(drag);
    }
}

/// Where `drag` puts the window with the finger at `screen`.
pub(crate) fn drag_rect(drag: &Drag, screen: (f32, f32)) -> RECT {
    let dx = (screen.0 - drag.from.0).round() as i32;
    let dy = (screen.1 - drag.from.1).round() as i32;
    let mut r = drag.rect;
    match drag.resize {
        None => {
            r.left += dx;
            r.right += dx;
            r.top += dy;
            r.bottom += dy;
        }
        Some(edge) => {
            if matches!(edge, WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT) {
                r.left += dx;
            }
            if matches!(edge, WMSZ_RIGHT | WMSZ_TOPRIGHT | WMSZ_BOTTOMRIGHT) {
                r.right += dx;
            }
            if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
                r.top += dy;
            }
            if matches!(edge, WMSZ_BOTTOM | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT) {
                r.bottom += dy;
            }
        }
    }
    r
}

/// Moves or resizes the window to follow the finger. A resize goes through `WM_SIZING` first,
/// so the window's own limits apply as in the system's size loop.
pub(crate) fn drag_to(hwnd: HWND, drag: &Drag, screen: (f32, f32)) {
    let mut rect = drag_rect(drag, screen);
    let mut flags = SWP_NOZORDER | SWP_NOACTIVATE;
    if let Some(edge) = drag.resize {
        // SAFETY: `rect` outlives the synchronous call.
        unsafe {
            SendMessageW(
                hwnd,
                WM_SIZING,
                Some(WPARAM(edge as usize)),
                Some(LPARAM(&mut rect as *mut RECT as isize)),
            );
        }
    } else {
        flags |= SWP_NOSIZE;
    }
    // SAFETY: plain arguments.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            flags,
        )
    };
}

/// One `POINTER_TOUCH_INFO`, in client device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TouchReading {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) screen: (f32, f32),
    pub(crate) pointer_flags: u32,
    pub(crate) touch_mask: u32,
    pub(crate) contact: (f32, f32),
    pub(crate) orientation: u32,
    pub(crate) pressure: u32,
}

impl TouchReading {
    pub(crate) fn position(&self, scale: f32) -> Point<Pixels> {
        point(px(self.x / scale), px(self.y / scale))
    }

    /// Only what the digitizer reports: no size, force or orientation it leaves out of its mask.
    pub(crate) fn event(&self, id: TouchId, phase: TouchPhase, scale: f32) -> TouchEvent {
        let has = |mask: u32| self.touch_mask & mask != 0;
        let (w, h) = self.contact;
        TouchEvent {
            id,
            phase,
            position: self.position(scale),
            predicted_position: None,
            force: has(TOUCH_MASK_PRESSURE)
                .then(|| (self.pressure as f32 / 1024.0).clamp(0.0, 1.0)),
            size: (has(TOUCH_MASK_CONTACTAREA) && w > 0.0 && h > 0.0)
                .then(|| size(px(w / scale), px(h / scale))),
            orientation: has(TOUCH_MASK_ORIENTATION).then(|| (self.orientation % 360) as f32),
        }
    }
}

/// What a touch message means for the contact, given whether it is being tracked. A contact the
/// system cancels (palm rejection, among others) never starts, or ends as cancelled.
pub(crate) fn phase(msg: u32, pointer_flags: u32, tracked: bool) -> Option<TouchPhase> {
    let flag = |f: POINTER_FLAGS| pointer_flags & f.0 != 0;
    if flag(POINTER_FLAG_CANCELED) {
        return tracked.then_some(TouchPhase::Cancelled);
    }
    match (msg, tracked) {
        (WM_POINTERDOWN, false) => Some(TouchPhase::Started),
        (WM_POINTERDOWN | WM_POINTERUPDATE, true) if flag(POINTER_FLAG_INCONTACT) => {
            Some(TouchPhase::Moved)
        }
        (WM_POINTERUPDATE | WM_POINTERUP, true) => Some(TouchPhase::Ended),
        (WM_POINTERLEAVE | WM_POINTERCAPTURECHANGED, true) => Some(TouchPhase::Cancelled),
        _ => None,
    }
}

/// The contact behind `pointer_id`. `None` when the pointer is not a finger, or is gone.
pub(crate) fn read(hwnd: HWND, pointer_id: u32) -> Option<TouchReading> {
    let mut info = POINTER_TOUCH_INFO::default();
    // SAFETY: plain out-parameter.
    unsafe { GetPointerTouchInfo(pointer_id, &mut info) }.ok()?;
    let p = &info.pointerInfo;
    let mut origin = POINT::default();
    // SAFETY: plain in/out parameter.
    let _ = unsafe { ClientToScreen(hwnd, &mut origin) };
    let mut device = RECT::default();
    let mut display = RECT::default();
    // SAFETY: plain out-parameters.
    let rects = unsafe { GetPointerDeviceRects(p.sourceDevice, &mut device, &mut display) }
        .ok()
        .map(|_| (device, display));
    let (x, y) = crate::pen::subpixel(p.ptPixelLocation, p.ptHimetricLocation, rects);
    let rc = info.rcContact;
    Some(TouchReading {
        x: x - origin.x as f32,
        y: y - origin.y as f32,
        screen: (x, y),
        pointer_flags: p.pointerFlags.0,
        touch_mask: info.touchMask,
        contact: ((rc.right - rc.left) as f32, (rc.bottom - rc.top) as f32),
        orientation: info.orientation,
        pressure: info.pressure,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use TouchPhase::*;

    #[test]
    fn messages_start_move_and_end_a_contact() {
        let contact = POINTER_FLAG_INCONTACT.0 | POINTER_FLAG_INRANGE.0;
        assert_eq!(phase(WM_POINTERDOWN, contact, false), Some(Started));
        assert_eq!(phase(WM_POINTERUPDATE, contact, true), Some(Moved));
        assert_eq!(phase(WM_POINTERUP, 0, true), Some(Ended));
        assert_eq!(phase(WM_POINTERUPDATE, 0, true), Some(Ended), "a lost up");
        assert_eq!(
            phase(WM_POINTERUPDATE, contact, false),
            None,
            "no down: never started"
        );
        assert_eq!(
            phase(WM_POINTERCAPTURECHANGED, contact, true),
            Some(Cancelled)
        );
        assert_eq!(phase(WM_POINTERLEAVE, 0, false), None);
    }

    #[test]
    fn a_cancelled_contact_never_starts_and_cancels_once_down() {
        let palm = POINTER_FLAG_INCONTACT.0 | POINTER_FLAG_CANCELED.0;
        assert_eq!(phase(WM_POINTERDOWN, palm, false), None);
        assert_eq!(phase(WM_POINTERUPDATE, palm, false), None);
        assert_eq!(phase(WM_POINTERUPDATE, palm, true), Some(Cancelled));
        assert_eq!(
            phase(WM_POINTERUP, POINTER_FLAG_CANCELED.0, true),
            Some(Cancelled)
        );
    }

    #[test]
    fn a_reading_carries_only_what_the_digitizer_reports() {
        let r = TouchReading {
            x: 300.0,
            y: 150.0,
            screen: (0.0, 0.0),
            pointer_flags: 0,
            touch_mask: 0,
            contact: (30.0, 45.0),
            orientation: 370,
            pressure: 512,
        };
        let bare = r.event(TouchId(7), Started, 1.5);
        assert_eq!(bare.position, point(px(200.0), px(100.0)));
        assert_eq!(
            (bare.force, bare.size, bare.orientation),
            (None, None, None)
        );

        let full = TouchReading {
            touch_mask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_PRESSURE | TOUCH_MASK_ORIENTATION,
            ..r
        }
        .event(TouchId(7), Moved, 1.5);
        assert_eq!(full.force, Some(0.5));
        assert_eq!(full.size, Some(size(px(20.0), px(30.0))));
        assert_eq!(full.orientation, Some(10.0));

        let flat = TouchReading {
            touch_mask: TOUCH_MASK_CONTACTAREA,
            contact: (0.0, 0.0),
            ..r
        }
        .event(TouchId(7), Moved, 1.0);
        assert_eq!(flat.size, None, "an empty rectangle is no size");
    }

    fn touch(phase: TouchPhase, x: f32) -> TouchEvent {
        TouchEvent {
            id: TouchId(0),
            phase,
            position: point(px(x), px(0.0)),
            predicted_position: None,
            force: None,
            size: None,
            orientation: None,
        }
    }

    #[test]
    fn ten_contacts_get_ten_ids_and_a_reused_pointer_id_a_fresh_one() {
        let tracker = TouchTracker::default();
        let at = touch(Started, 0.0);
        let ids: Vec<TouchId> = (0..10).map(|p| tracker.start(p, &at, (0.0, 0.0))).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 10);
        assert!((0..10).all(|p| tracker.find(p).is_some()));
        tracker.end(3);
        assert!(tracker.find(3).is_none());
        let again = tracker.start(3, &at, (0.0, 0.0));
        assert!(!ids.contains(&again));
    }

    #[test]
    fn a_finger_held_still_reports_no_move() {
        let tracker = TouchTracker::default();
        tracker.start(1, &touch(Started, 5.0), (0.0, 0.0));
        assert!(!tracker.moved(1, &touch(Moved, 5.0), (0.0, 0.0)));
        assert!(tracker.moved(1, &touch(Moved, 6.0), (0.0, 0.0)));
        let pressed = TouchEvent {
            force: Some(0.7),
            ..touch(Moved, 6.0)
        };
        assert!(tracker.moved(1, &pressed, (0.0, 0.0)));
        assert!(!tracker.moved(1, &pressed, (0.0, 0.0)));
    }

    #[test]
    fn an_end_the_window_could_not_take_comes_back_as_a_cancel() {
        let tracker = TouchTracker::default();
        assert!(tracker.outbox(touch(Moved, 1.0), false).is_empty());
        assert!(tracker.outbox(touch(Ended, 1.0), false).is_empty());
        let out = tracker.outbox(touch(Started, 2.0), true);
        let phases: Vec<TouchPhase> = out.iter().map(|e| e.phase).collect();
        assert_eq!(phases, [Cancelled, Started]);
        assert_eq!(tracker.outbox(touch(Moved, 2.0), true).len(), 1);
    }

    #[test]
    fn a_finger_drag_moves_the_window_or_the_edge_it_holds() {
        let drag = Drag {
            pointer_id: 1,
            from: (100.0, 100.0),
            rect: RECT {
                left: 0,
                top: 0,
                right: 400,
                bottom: 300,
            },
            resize: None,
        };
        let moved = drag_rect(&drag, (130.0, 90.0));
        assert_eq!(
            (moved.left, moved.top, moved.right, moved.bottom),
            (30, -10, 430, 290)
        );
        let corner = drag_rect(
            &Drag {
                resize: Some(WMSZ_TOPLEFT),
                ..drag
            },
            (130.0, 90.0),
        );
        assert_eq!(
            (corner.left, corner.top, corner.right, corner.bottom),
            (30, -10, 400, 300)
        );
    }
}
