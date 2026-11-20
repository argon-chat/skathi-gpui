//! The native pointer ID belongs to the window receiving WM_POINTER. Keep the
//! selected controller with a queued effect so a later input cannot redirect it.
use gpui::{HapticPattern, HapticTarget};
use windows::Devices::{
    Haptics::{SimpleHapticsController as Controller, SimpleHapticsControllerFeedback as Feedback},
    Input::PenDevice,
};

struct Effect {
    controller: Controller,
    feedback: Feedback,
    continuous: bool,
}

#[derive(Default)]
pub(crate) struct WindowHaptics {
    pen: Option<(u32, bool)>,
    pen_controller: Option<Controller>,
    // A single frame can cross several detents. Play the latest one for each device.
    pending_mouse: Option<Effect>,
    pending_pen: Option<Effect>,
    active: Option<Controller>,
}

impl WindowHaptics {
    pub(crate) fn observe_pen(&mut self, id: u32, contact: bool) {
        if let Some((previous, was_down)) = self.pen {
            if previous != id || (was_down && !contact) {
                self.pen_ended(previous, previous != id);
            }
        }
        if !self.pen.is_some_and(|p| p.0 == id)
            || (contact && self.pen == Some((id, false)) && self.pen_controller.is_none())
        {
            // The manager follows the thread's last device. Capture it during this
            // pen's own WM_POINTER, before another window or pen can change it.
            self.pen_controller = PenDevice::GetFromPointerId(id)
                .and_then(|p| p.SimpleHapticsController())
                .ok()
                .or_else(|| {
                    let (kind, controller) = manager::current()?;
                    (kind == manager::PEN).then_some(controller)
                });
        }
        self.pen = Some((id, contact));
    }

    pub(crate) fn pen_ended(&mut self, id: u32, left: bool) {
        if self.pen.is_some_and(|p| p.0 == id) {
            if left || self.pending_pen.as_ref().is_some_and(|e| e.continuous) {
                self.pending_pen = None;
            }
            if let Some(controller) = self.active.take() {
                let _ = controller.StopFeedback();
            }
            self.pen = (!left).then_some((id, false));
            if left {
                self.pen_controller = None;
            }
        }
    }

    fn controller(&self, target: HapticTarget) -> Option<(Option<u32>, Controller)> {
        let pen = || Some((Some(self.pen?.0), self.pen_controller.clone()?));
        if target == HapticTarget::Pen {
            return pen();
        }
        match manager::current() {
            Some((manager::GENERIC | manager::MOUSE | manager::TOUCHPAD, controller)) => {
                Some((None, controller))
            }
            _ if target == HapticTarget::Current => pen(),
            _ => None,
        }
    }

    pub(crate) fn supported(&self, target: HapticTarget) -> bool {
        self.controller(target).is_some_and(|(_, c)| {
            c.SupportedFeedback()
                .and_then(|f| f.Size())
                .is_ok_and(|n| n > 0)
        })
    }

    pub(crate) fn patterns(&self, target: HapticTarget) -> Vec<HapticPattern> {
        let Some((pen, controller)) = self.controller(target) else {
            return Vec::new();
        };
        let feedbacks = advertised(&controller);
        HapticPattern::NAMED
            .iter()
            .map(|&(_, p)| p)
            .filter(|p| {
                (!p.is_continuous() || target == HapticTarget::Pen)
                    && feedback_for(*p, pen.is_some(), &feedbacks).is_some()
            })
            .collect()
    }

    pub(crate) fn perform(
        &mut self,
        pattern: HapticPattern,
        target: HapticTarget,
        deferred: bool,
    ) -> bool {
        use HapticPattern::*;
        let candidates: &[HapticPattern] = match pattern {
            Alignment => &[Alignment, Generic],
            LevelChange => &[LevelChange, Press, Generic],
            PencilContinuous => &[PencilContinuous, InkContinuous],
            _ => std::slice::from_ref(&pattern),
        };
        self.perform_first(candidates, target, deferred)
    }

    pub(crate) fn perform_first(
        &mut self,
        patterns: &[HapticPattern],
        target: HapticTarget,
        deferred: bool,
    ) -> bool {
        let Some((pen, controller)) = self.controller(target) else {
            return false;
        };
        let feedbacks = advertised(&controller);
        let can_draw = target == HapticTarget::Pen && self.pen.is_some_and(|p| p.1);
        let Some((feedback, continuous)) = patterns.iter().find_map(|&p| {
            if p.is_continuous() && !can_draw {
                return None;
            }
            feedback_for(p, pen.is_some(), &feedbacks).map(|f| (f.clone(), p.is_continuous()))
        }) else {
            return false;
        };
        let effect = Effect {
            controller,
            feedback,
            continuous,
        };
        if deferred {
            if pen.is_some() {
                self.pending_pen = Some(effect);
            } else {
                self.pending_mouse = Some(effect);
            }
            true
        } else {
            self.play(effect)
        }
    }

    fn play(&mut self, effect: Effect) -> bool {
        if effect
            .controller
            .SendHapticFeedback(&effect.feedback)
            .is_err()
        {
            return false;
        }
        if effect.continuous {
            self.active = Some(effect.controller);
        }
        true
    }

    pub(crate) fn presented(&mut self) {
        for effect in [self.pending_mouse.take(), self.pending_pen.take()]
            .into_iter()
            .flatten()
        {
            self.play(effect);
        }
    }

    pub(crate) fn stop(&mut self, target: HapticTarget) -> bool {
        let mut stopped = false;
        if target != HapticTarget::Pen {
            stopped |= self.pending_mouse.take().is_some();
        }
        if target != HapticTarget::Mouse {
            stopped |= self.pending_pen.take().is_some();
            if let Some(controller) = self.active.take() {
                stopped |= controller.StopFeedback().is_ok();
            }
        }
        stopped
    }

    pub(crate) fn clear(&mut self) {
        self.stop(HapticTarget::Current);
        self.pen = None;
        self.pen_controller = None;
    }
}

fn advertised(controller: &Controller) -> Vec<(u16, Feedback)> {
    let Ok(feedbacks) = controller.SupportedFeedback() else {
        return Vec::new();
    };
    feedbacks
        .into_iter()
        .filter_map(|f| Some((f.Waveform().ok()?, f)))
        .collect()
}

fn feedback_for<T>(pattern: HapticPattern, pen: bool, feedbacks: &[(u16, T)]) -> Option<&T> {
    use HapticPattern::*;
    // SimpleHapticsControllerFeedback.Waveform is a USB HID Haptics-page usage.
    const HOVER: u16 = 0x1008;
    const CLICK: u16 = 0x1003;
    let waves: &[u16] = match pattern {
        Generic if !pen => &[HOVER, CLICK],
        Generic | Click => &[CLICK],
        Hover => &[HOVER],
        Press => &[0x1006],
        Release => &[0x1007],
        Success => &[0x1009],
        Error => &[0x100a],
        InkContinuous => &[0x100b],
        PencilContinuous => &[0x100c],
        Collide => &[0x1012],
        Alignment => &[0x1013],
        LevelChange => &[0x1014],
        Grow => &[0x1015],
    };
    waves
        .iter()
        .find_map(|wave| feedbacks.iter().find(|(id, _)| id == wave).map(|(_, f)| f))
}

// windows-rs 0.62 predates the desktop haptics interfaces. These are the
// vtable prefixes from Windows.Devices.Haptics.winmd (UniversalApiContract 19).
// Activation and QueryInterface keep earlier versions of Windows loadable.
mod manager {
    use super::Controller;
    use core::ffi::c_void;
    use windows_core::{HRESULT, IInspectable_Vtbl, Interface, RuntimeName, factory};

    pub(super) const GENERIC: i32 = 1;
    pub(super) const PEN: i32 = 2;
    pub(super) const TOUCHPAD: i32 = 3;
    pub(super) const MOUSE: i32 = 4;

    struct InputManager;
    impl RuntimeName for InputManager {
        const NAME: &'static str = "Windows.Devices.Haptics.InputHapticsManager";
    }

    type ObjectGetter = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
    type BoolGetter = unsafe extern "system" fn(*mut c_void, *mut bool) -> HRESULT;

    windows_core::imp::define_interface!(
        Statics,
        StaticsVtbl,
        0x7bb40f77_e187_5322_844e_aa58223c281a
    );
    #[repr(C)]
    pub struct StaticsVtbl {
        base: IInspectable_Vtbl,
        supported: BoolGetter,
        present: BoolGetter,
        current_thread: ObjectGetter,
    }

    windows_core::imp::define_interface!(
        Manager,
        ManagerVtbl,
        0x040e91df_bb3a_507c_9e25_a2d2c685b2e5
    );
    #[repr(C)]
    pub struct ManagerVtbl {
        base: IInspectable_Vtbl,
        thread_id: usize,
        device_type: unsafe extern "system" fn(*mut c_void, *mut i32) -> HRESULT,
        controller: ObjectGetter,
    }

    /// `get` must be this owner's WinRT getter for an owned interface of type `I`.
    unsafe fn object<I: Interface>(owner: &impl Interface, get: ObjectGetter) -> Option<I> {
        let mut raw = core::ptr::null_mut();
        // Successful WinRT object getters transfer one reference to the caller.
        unsafe {
            get(owner.as_raw(), &mut raw).ok().ok()?;
            (!raw.is_null()).then(|| I::from_raw(raw))
        }
    }

    pub(super) fn current() -> Option<(i32, Controller)> {
        let api = factory::<InputManager, Statics>().ok()?;
        let mut available = false;
        unsafe {
            (api.vtable().supported)(api.as_raw(), &mut available)
                .ok()
                .ok()?;
            if !available {
                return None;
            }
            (api.vtable().present)(api.as_raw(), &mut available)
                .ok()
                .ok()?;
            if !available {
                return None;
            }
            let manager: Manager = object(&api, api.vtable().current_thread)?;
            let mut device = 0;
            (manager.vtable().device_type)(manager.as_raw(), &mut device)
                .ok()
                .ok()?;
            Some((device, object(&manager, manager.vtable().controller)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_and_strict_fallback_use_the_advertised_mouse_waveforms() {
        use HapticPattern::*;
        let mouse = [(0x1013, Alignment), (0x1008, Hover), (0x1009, Success)];
        assert_eq!(feedback_for(Generic, false, &mouse), Some(&Hover));
        assert_eq!(feedback_for(Click, false, &mouse), None);
        assert_eq!(feedback_for(Alignment, false, &mouse), Some(&Alignment));
        assert_eq!(
            [Click, Success, Generic]
                .into_iter()
                .find_map(|p| feedback_for(p, false, &mouse)),
            Some(&Success)
        );
        assert_eq!(feedback_for(Press, false, &mouse), None);
    }

    #[test]
    fn pens_use_click_and_keep_continuous_feedback_on_stroke_waveforms() {
        use HapticPattern::*;
        let pen = [(0x1003, Click), (0x100b, InkContinuous), (0x1008, Hover)];
        assert_eq!(feedback_for(Generic, true, &pen), Some(&Click));
        assert_eq!(
            feedback_for(InkContinuous, true, &pen),
            Some(&InkContinuous)
        );
        assert_eq!(feedback_for(PencilContinuous, true, &pen), None);
    }

    #[test]
    fn a_pen_that_left_cannot_be_addressed_by_its_previous_id() {
        let mut h = WindowHaptics::default();
        h.observe_pen(73, true);
        h.observe_pen(89, true);
        h.pen_ended(73, true);
        assert_eq!(h.pen, Some((89, true)));
        h.pen_ended(89, false);
        assert!(!h.perform(HapticPattern::InkContinuous, HapticTarget::Pen, false));
        h.pen_ended(89, true);
        assert!(!h.supported(HapticTarget::Pen));
    }
}
