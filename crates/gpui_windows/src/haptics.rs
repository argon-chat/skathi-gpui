//! The native pointer ID belongs to the window receiving WM_POINTER. Keep the
//! selected controller with a queued effect so a later input cannot redirect it.
use gpui::{HapticPattern, HapticTarget};
use windows::Devices::{
    Haptics::{
        KnownSimpleHapticsControllerWaveforms as Waves, SimpleHapticsController as Controller,
        SimpleHapticsControllerFeedback as Feedback,
    },
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

    pub(crate) fn perform(
        &mut self,
        pattern: HapticPattern,
        target: HapticTarget,
        deferred: bool,
    ) -> bool {
        let continuous = pattern.is_continuous();
        if continuous && (target != HapticTarget::Pen || !self.pen.is_some_and(|p| p.1)) {
            return false;
        }
        let Some((pen, controller)) = self.controller(target) else {
            return false;
        };
        let Ok(feedbacks) = controller.SupportedFeedback() else {
            return false;
        };
        let Some(feedback) = waveforms(pattern, pen.is_some()).find_map(|wave| {
            feedbacks
                .clone()
                .into_iter()
                .find(|f| f.Waveform().ok() == Some(wave))
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

fn waveforms(pattern: HapticPattern, pen: bool) -> impl Iterator<Item = u16> {
    let preferred = match pattern {
        HapticPattern::Generic => [None, None],
        HapticPattern::Alignment => [manager::detent(true), None],
        HapticPattern::LevelChange => [manager::detent(false), Waves::Press().ok()],
        HapticPattern::InkContinuous => [Waves::InkContinuous().ok(), None],
        HapticPattern::PencilContinuous => {
            [Waves::PencilContinuous().ok(), Waves::InkContinuous().ok()]
        }
    };
    // Windows defaults to Hover on mice and touchpads, and Click on pens.
    let fallback = match (pattern.is_continuous(), pen) {
        (true, _) => [None, None],
        (false, true) => [Waves::Click().ok(), None],
        (false, false) => [Waves::Hover().ok(), Waves::Click().ok()],
    };
    preferred.into_iter().chain(fallback).flatten()
}

// windows-rs 0.62 predates the desktop haptics interfaces. These are the
// vtable prefixes from Windows.Devices.Haptics.winmd (UniversalApiContract 19).
// Activation and QueryInterface keep earlier versions of Windows loadable.
mod manager {
    use super::{Controller, Waves};
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
    type WaveGetter = unsafe extern "system" fn(*mut c_void, *mut u16) -> HRESULT;

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

    windows_core::imp::define_interface!(
        Detents,
        DetentsVtbl,
        0xae480ce4_4ab6_5b2f_ad0b_cb52f37d45fb
    );
    #[repr(C)]
    pub struct DetentsVtbl {
        base: IInspectable_Vtbl,
        collide: usize,
        align: WaveGetter,
        step: WaveGetter,
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

    pub(super) fn detent(alignment: bool) -> Option<u16> {
        let api = factory::<Waves, Detents>().ok()?;
        let mut wave = 0;
        unsafe {
            let get = if alignment {
                api.vtable().align
            } else {
                api.vtable().step
            };
            get(api.as_raw(), &mut wave).ok().ok()?;
        }
        Some(wave)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_and_touchpad_pulses_work_without_click() {
        let hover = Waves::Hover().unwrap();
        let click = Waves::Click().unwrap();
        assert!(waveforms(HapticPattern::Generic, false).eq([hover, click]));
        for pattern in [HapticPattern::Alignment, HapticPattern::LevelChange] {
            assert_eq!(
                waveforms(pattern, false).find(|&id| id == hover),
                Some(hover)
            );
        }
    }

    #[test]
    fn pens_use_click_and_keep_continuous_feedback_on_stroke_waveforms() {
        let click = Waves::Click().unwrap();
        assert!(waveforms(HapticPattern::Generic, true).eq([click]));
        assert_eq!(
            waveforms(HapticPattern::Alignment, true).last(),
            Some(click)
        );
        assert!(waveforms(HapticPattern::PencilContinuous, true).eq([
            Waves::PencilContinuous().unwrap(),
            Waves::InkContinuous().unwrap()
        ]));
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
