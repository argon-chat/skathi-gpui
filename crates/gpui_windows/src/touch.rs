//! Touch contacts. A finger's `WM_POINTER*` messages are handled here rather than left to
//! `DefWindowProc`, which would turn the first contact into mouse messages and run the system's
//! touch gestures over it.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Pixels, Point, TouchEvent, TouchId, TouchPhase, point, px, size};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::{Input::Pointer::*, WindowsAndMessaging::*},
};

/// Windows reuses pointer ids; a [`TouchId`] is never reused.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Tracked {
    pointer_id: u32,
    id: TouchId,
    position: Point<Pixels>,
}

/// The contacts down in one window, any number of them.
#[derive(Default)]
pub(crate) struct TouchTracker {
    down: RefCell<Vec<Tracked>>,
}

impl TouchTracker {
    pub(crate) fn find(&self, pointer_id: u32) -> Option<(TouchId, Point<Pixels>)> {
        self.down
            .borrow()
            .iter()
            .find(|t| t.pointer_id == pointer_id)
            .map(|t| (t.id, t.position))
    }

    pub(crate) fn start(&self, pointer_id: u32, position: Point<Pixels>) -> TouchId {
        let id = TouchId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        let mut down = self.down.borrow_mut();
        down.retain(|t| t.pointer_id != pointer_id);
        down.push(Tracked {
            pointer_id,
            id,
            position,
        });
        id
    }

    pub(crate) fn moved(&self, pointer_id: u32, position: Point<Pixels>) {
        if let Some(t) = self
            .down
            .borrow_mut()
            .iter_mut()
            .find(|t| t.pointer_id == pointer_id)
        {
            t.position = position;
        }
    }

    pub(crate) fn end(&self, pointer_id: u32) {
        self.down
            .borrow_mut()
            .retain(|t| t.pointer_id != pointer_id);
    }
}

/// One `POINTER_TOUCH_INFO`, in client device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TouchReading {
    pub(crate) x: f32,
    pub(crate) y: f32,
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

    #[test]
    fn ten_contacts_get_ten_ids_and_a_reused_pointer_id_a_fresh_one() {
        let tracker = TouchTracker::default();
        let at = point(px(0.0), px(0.0));
        let ids: Vec<TouchId> = (0..10).map(|p| tracker.start(p, at)).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 10);
        assert!((0..10).all(|p| tracker.find(p).is_some()));
        tracker.end(3);
        assert!(tracker.find(3).is_none());
        let again = tracker.start(3, at);
        assert!(!ids.contains(&again));
    }
}
