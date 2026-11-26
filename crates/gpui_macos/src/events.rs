use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, MousePressureEvent, MouseUpEvent,
    NavigationDirection, PenAction, PenEvent, PenInput, PenSample, PinchEvent, Pixels,
    PlatformInput, Point, PressureStage, ScrollDelta, ScrollEvent, ScrollSource, TouchPhase, point,
    px,
};
use std::cell::Cell;

thread_local! {
    pub(crate) static HAS_FORCE_TOUCH: Cell<bool> = const { Cell::new(false) };
}

use crate::{
    LMGetKbdType, NSStringExt, TISCopyCurrentKeyboardLayoutInputSource, TISGetInputSourceProperty,
    UCKeyTranslate, kTISPropertyUnicodeKeyLayoutData,
};
use core_foundation::data::{CFDataGetBytePtr, CFDataRef};
use core_graphics::event::CGKeyCode;
use objc2::{msg_send, runtime::AnyObject};
use objc2_app_kit::{
    NSDeleteFunctionKey, NSDownArrowFunctionKey, NSEndFunctionKey, NSEvent, NSEventButtonMask,
    NSEventModifierFlags, NSEventPhase, NSEventSubtype, NSEventType, NSF1FunctionKey,
    NSF2FunctionKey, NSF3FunctionKey, NSF4FunctionKey, NSF5FunctionKey, NSF6FunctionKey,
    NSF7FunctionKey, NSF8FunctionKey, NSF9FunctionKey, NSF10FunctionKey, NSF11FunctionKey,
    NSF12FunctionKey, NSF13FunctionKey, NSF14FunctionKey, NSF15FunctionKey, NSF16FunctionKey,
    NSF17FunctionKey, NSF18FunctionKey, NSF19FunctionKey, NSF20FunctionKey, NSF21FunctionKey,
    NSF22FunctionKey, NSF23FunctionKey, NSF24FunctionKey, NSF25FunctionKey, NSF26FunctionKey,
    NSF27FunctionKey, NSF28FunctionKey, NSF29FunctionKey, NSF30FunctionKey, NSF31FunctionKey,
    NSF32FunctionKey, NSF33FunctionKey, NSF34FunctionKey, NSF35FunctionKey, NSHelpFunctionKey,
    NSHomeFunctionKey, NSLeftArrowFunctionKey, NSModeSwitchFunctionKey, NSPageDownFunctionKey,
    NSPageUpFunctionKey, NSPointingDeviceType, NSRightArrowFunctionKey, NSUpArrowFunctionKey,
};
use objc2_foundation::NSProcessInfo;
use std::time::{Duration, Instant};
use std::{borrow::Cow, ffi::c_void};

pub(crate) const ESCAPE_KEY: u16 = 0x1b;

const BACKSPACE_KEY: u32 = 0x7f;
const SPACE_KEY: u32 = b' ' as u32;
const ENTER_KEY: u32 = 0x0d;
const NUMPAD_ENTER_KEY: u32 = 0x03;
const TAB_KEY: u32 = 0x09;
const SHIFT_TAB_KEY: u32 = 0x19;

// Menu key equivalents use different character values than the ones NSEvent
// reports for the same physical keys.
//
// See https://developer.apple.com/documentation/appkit/nsmenuitem/keyequivalent#Discussion
const BACKSPACE_KEY_EQUIVALENT: u32 = 0x08;
const DELETE_KEY_EQUIVALENT: u32 = 0x7f;

pub(crate) fn key_to_menu_equivalent(key: &str) -> Cow<'_, str> {
    let code = match key {
        "space" => SPACE_KEY,
        "backspace" => BACKSPACE_KEY_EQUIVALENT,
        "escape" => u32::from(ESCAPE_KEY),
        "up" => NSUpArrowFunctionKey,
        "down" => NSDownArrowFunctionKey,
        "left" => NSLeftArrowFunctionKey,
        "right" => NSRightArrowFunctionKey,
        "pageup" => NSPageUpFunctionKey,
        "pagedown" => NSPageDownFunctionKey,
        "home" => NSHomeFunctionKey,
        "end" => NSEndFunctionKey,
        "delete" => DELETE_KEY_EQUIVALENT,
        "insert" => NSHelpFunctionKey,
        "f1" => NSF1FunctionKey,
        "f2" => NSF2FunctionKey,
        "f3" => NSF3FunctionKey,
        "f4" => NSF4FunctionKey,
        "f5" => NSF5FunctionKey,
        "f6" => NSF6FunctionKey,
        "f7" => NSF7FunctionKey,
        "f8" => NSF8FunctionKey,
        "f9" => NSF9FunctionKey,
        "f10" => NSF10FunctionKey,
        "f11" => NSF11FunctionKey,
        "f12" => NSF12FunctionKey,
        "f13" => NSF13FunctionKey,
        "f14" => NSF14FunctionKey,
        "f15" => NSF15FunctionKey,
        "f16" => NSF16FunctionKey,
        "f17" => NSF17FunctionKey,
        "f18" => NSF18FunctionKey,
        "f19" => NSF19FunctionKey,
        "f20" => NSF20FunctionKey,
        "f21" => NSF21FunctionKey,
        "f22" => NSF22FunctionKey,
        "f23" => NSF23FunctionKey,
        "f24" => NSF24FunctionKey,
        "f25" => NSF25FunctionKey,
        "f26" => NSF26FunctionKey,
        "f27" => NSF27FunctionKey,
        "f28" => NSF28FunctionKey,
        "f29" => NSF29FunctionKey,
        "f30" => NSF30FunctionKey,
        "f31" => NSF31FunctionKey,
        "f32" => NSF32FunctionKey,
        "f33" => NSF33FunctionKey,
        "f34" => NSF34FunctionKey,
        "f35" => NSF35FunctionKey,
        _ => return Cow::Borrowed(key),
    };
    Cow::Owned(String::from_utf16(&[code as u16]).unwrap())
}

fn read_modifiers(native_event: &NSEvent) -> Modifiers {
    let modifiers = native_event.modifierFlags();
    let control = modifiers.contains(NSEventModifierFlags::Control);
    let alt = modifiers.contains(NSEventModifierFlags::Option);
    let shift = modifiers.contains(NSEventModifierFlags::Shift);
    let command = modifiers.contains(NSEventModifierFlags::Command);
    let function = modifiers.contains(NSEventModifierFlags::Function);

    Modifiers {
        control,
        alt,
        shift,
        platform: command,
        function,
    }
}

fn native_key(event: &NSEvent) -> gpui::NativeKey {
    let printable = |text: &String| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| !c.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&c))
    };
    let text = event.characters().map(|s| s.to_string());
    let dead = text.as_deref() == Some("");
    let key = text.filter(printable).or_else(|| {
        event
            .charactersIgnoringModifiers()
            .map(|s| s.to_string())
            .filter(printable)
    });
    gpui::NativeKey {
        code: gpui::NativeKeyCode::Mac(event.keyCode()),
        key: if dead { Some("Dead".into()) } else { key },
        modifiers: read_modifiers(event),
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::*;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSPoint, NSString};

    #[test]
    fn appkit_physical_keyboard_events() {
        objc2::rc::autoreleasepool(|_| {
            let make = |kind: NSEventType,
                        code: u16,
                        text: &str,
                        flags: NSEventModifierFlags,
                        repeat: bool| {
                let text = NSString::from_str(text);
                let event: *mut AnyObject = unsafe {
                    msg_send![objc2::class!(NSEvent),
                        keyEventWithType: kind.0,
                        location: NSPoint::default(),
                        modifierFlags: flags.bits(),
                        timestamp: 0.0_f64,
                        windowNumber: 0_isize,
                        context: std::ptr::null::<AnyObject>(),
                        characters: &*text,
                        charactersIgnoringModifiers: &*text,
                        isARepeat: Bool::new(repeat),
                        keyCode: code]
                };
                assert!(!event.is_null());
                event
            };
            let shift = NSEventModifierFlags::Shift;
            for (code, text) in [(37, "L"), (18, "!")] {
                let ev = make(NSEventType::KeyDown, code, text, shift, false);
                let key = native_key(unsafe { &*ev.cast::<NSEvent>() });
                assert_eq!(key.code, gpui::NativeKeyCode::Mac(code));
                assert_eq!(key.key.as_deref(), Some(text));
                assert!(key.modifiers.shift);
            }
            for (kind, repeat) in [
                (NSEventType::KeyDown, false),
                (NSEventType::KeyDown, true),
                (NSEventType::KeyUp, false),
            ] {
                let ev = make(kind, 49, " ", NSEventModifierFlags::empty(), repeat);
                let native = match unsafe { platform_input_from_native(ev, None) }.unwrap() {
                    PlatformInput::KeyDown(e) => {
                        assert_eq!(e.is_held, repeat);
                        e.native
                    }
                    PlatformInput::KeyUp(e) => {
                        assert_eq!(kind, NSEventType::KeyUp);
                        e.native
                    }
                    _ => panic!("expected a key event"),
                }
                .unwrap();
                assert_eq!(native.code, gpui::NativeKeyCode::Mac(49));
                assert_eq!(native.key.as_deref(), Some(" "));
            }
            // A left Shift release while right Shift remains pressed retains the aggregate flag.
            let ev = make(
                NSEventType::FlagsChanged,
                56,
                "",
                shift | NSEventModifierFlags::from_bits_retain(4),
                false,
            );
            let Some(PlatformInput::KeyUp(key)) = (unsafe { modifier_key(ev) }) else {
                panic!("expected left shift release")
            };
            assert!(key.native.unwrap().modifiers.shift);
        });
    }
}

/// AppKit reports physical modifier edges through flagsChanged, including opposite-side chords.
pub(crate) unsafe fn modifier_key(native_event: *mut AnyObject) -> Option<PlatformInput> {
    let event = unsafe { native_event.cast::<NSEvent>().as_ref()? };
    let code = event.keyCode();
    let flags = event.modifierFlags();
    let (key, aggregate, side, pair) = match code {
        56 => ("Shift", NSEventModifierFlags::Shift, 0x0002, 0x0006),
        60 => ("Shift", NSEventModifierFlags::Shift, 0x0004, 0x0006),
        59 => ("Control", NSEventModifierFlags::Control, 0x0001, 0x2001),
        62 => ("Control", NSEventModifierFlags::Control, 0x2000, 0x2001),
        58 => ("Alt", NSEventModifierFlags::Option, 0x0020, 0x0060),
        61 => ("Alt", NSEventModifierFlags::Option, 0x0040, 0x0060),
        55 => ("Meta", NSEventModifierFlags::Command, 0x0008, 0x0018),
        54 => ("Meta", NSEventModifierFlags::Command, 0x0010, 0x0018),
        57 => ("CapsLock", NSEventModifierFlags::CapsLock, 0, 0),
        63 => ("Fn", NSEventModifierFlags::Function, 0, 0),
        _ => return None,
    };
    let down = if flags.bits() & pair != 0 {
        flags.bits() & side != 0
    } else {
        flags.contains(aggregate)
    };
    Some(gpui::NativeKey::modifier_event(
        gpui::NativeKeyCode::Mac(code),
        key,
        read_modifiers(event),
        down,
        false,
    ))
}

pub(crate) unsafe fn platform_input_from_native(
    native_event: *mut AnyObject,
    window_height: Option<Pixels>,
) -> Option<PlatformInput> {
    unsafe {
        let native_event = native_event.cast::<NSEvent>().as_ref()?;
        let event_type = native_event.r#type();

        // Filter out event types that aren't in the NSEventType enum.
        // See https://github.com/servo/cocoa-rs/issues/155#issuecomment-323482792 for details.
        match event_type.0 {
            0 | 21 | 32 | 33 | 35 | 36 | 37 => {
                return None;
            }
            _ => {}
        }

        match event_type {
            NSEventType::FlagsChanged => {
                Some(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                    modifiers: read_modifiers(native_event),
                    capslock: Capslock {
                        on: native_event
                            .modifierFlags()
                            .contains(NSEventModifierFlags::CapsLock),
                    },
                }))
            }
            NSEventType::KeyDown => Some(PlatformInput::KeyDown(KeyDownEvent {
                keystroke: parse_keystroke(native_event),
                native: Some(native_key(native_event)),
                is_held: native_event.isARepeat(),
                prefer_character_input: false,
            })),
            NSEventType::KeyUp => Some(PlatformInput::KeyUp(KeyUpEvent {
                keystroke: parse_keystroke(native_event),
                native: Some(native_key(native_event)),
            })),
            NSEventType::LeftMouseDown
            | NSEventType::RightMouseDown
            | NSEventType::OtherMouseDown => {
                let button = match native_event.buttonNumber() {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Navigate(NavigationDirection::Back),
                    4 => MouseButton::Navigate(NavigationDirection::Forward),
                    // Other mouse buttons aren't tracked currently
                    _ => return None,
                };
                window_height.map(|window_height| {
                    PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            // MacOS screen coordinates are relative to bottom left
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                        modifiers: read_modifiers(native_event),
                        click_count: native_event.clickCount() as usize,
                        first_mouse: false,
                    })
                })
            }
            NSEventType::LeftMouseUp | NSEventType::RightMouseUp | NSEventType::OtherMouseUp => {
                let button = match native_event.buttonNumber() {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Navigate(NavigationDirection::Back),
                    4 => MouseButton::Navigate(NavigationDirection::Forward),
                    // Other mouse buttons aren't tracked currently
                    _ => return None,
                };

                window_height.map(|window_height| {
                    PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                        modifiers: read_modifiers(native_event),
                        click_count: native_event.clickCount() as usize,
                    })
                })
            }
            NSEventType::Pressure => {
                let stage = native_event.stage();
                if stage > 0 {
                    HAS_FORCE_TOUCH.set(true);
                }
                let pressure = native_event.pressure();

                window_height.map(|window_height| {
                    PlatformInput::MousePressure(MousePressureEvent {
                        stage: match stage {
                            1 => PressureStage::Normal,
                            2 => PressureStage::Force,
                            _ => PressureStage::Zero,
                        },
                        pressure,
                        modifiers: read_modifiers(native_event),
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                    })
                })
            }
            // Some mice (like Logitech MX Master) send navigation buttons as swipe events
            NSEventType::Swipe => {
                let navigation_direction = match native_event.phase() {
                    NSEventPhase::Ended => match native_event.deltaX() {
                        x if x > 0.0 => Some(NavigationDirection::Back),
                        x if x < 0.0 => Some(NavigationDirection::Forward),
                        _ => return None,
                    },
                    _ => return None,
                };

                match navigation_direction {
                    Some(direction) => window_height.map(|window_height| {
                        PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Navigate(direction),
                            position: point(
                                px(native_event.locationInWindow().x as f32),
                                window_height - px(native_event.locationInWindow().y as f32),
                            ),
                            modifiers: read_modifiers(native_event),
                            click_count: 1,
                            first_mouse: false,
                        })
                    }),
                    _ => None,
                }
            }
            NSEventType::Magnify => window_height.map(|window_height| {
                let phase = match native_event.phase() {
                    NSEventPhase::MayBegin | NSEventPhase::Began => TouchPhase::Started,
                    NSEventPhase::Ended => TouchPhase::Ended,
                    NSEventPhase::Cancelled => TouchPhase::Cancelled,
                    _ => TouchPhase::Moved,
                };

                let magnification = native_event.magnification() as f32;

                PlatformInput::Pinch(PinchEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),
                    delta: magnification,
                    modifiers: read_modifiers(native_event),
                    phase,
                })
            }),
            NSEventType::ScrollWheel => window_height.map(|window_height| {
                let phase = match native_event.phase() {
                    NSEventPhase::MayBegin | NSEventPhase::Began => TouchPhase::Started,
                    NSEventPhase::Ended => TouchPhase::Ended,
                    _ => TouchPhase::Moved,
                };

                let raw_data = point(
                    native_event.scrollingDeltaX() as f32,
                    native_event.scrollingDeltaY() as f32,
                );

                let delta = if native_event.hasPreciseScrollingDeltas() {
                    ScrollDelta::Pixels(raw_data.map(px))
                } else {
                    ScrollDelta::Lines(raw_data)
                };

                PlatformInput::Scroll(ScrollEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),
                    delta,
                    touch_phase: phase,
                    modifiers: read_modifiers(native_event),
                    source: ScrollSource::Pointer,
                })
            }),
            NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged => {
                let pressed_button = match native_event.buttonNumber() {
                    0 => MouseButton::Left,
                    1 => MouseButton::Right,
                    2 => MouseButton::Middle,
                    3 => MouseButton::Navigate(NavigationDirection::Back),
                    4 => MouseButton::Navigate(NavigationDirection::Forward),
                    // Other mouse buttons aren't tracked currently
                    _ => return None,
                };

                window_height.map(|window_height| {
                    PlatformInput::MouseMove(MouseMoveEvent {
                        pressed_button: Some(pressed_button),
                        position: point(
                            px(native_event.locationInWindow().x as f32),
                            window_height - px(native_event.locationInWindow().y as f32),
                        ),
                        modifiers: read_modifiers(native_event),
                    })
                })
            }
            NSEventType::MouseMoved => window_height.map(|window_height| {
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),
                    pressed_button: None,
                    modifiers: read_modifiers(native_event),
                })
            }),
            NSEventType::TabletProximity => note_proximity(native_event),
            NSEventType::MouseExited => window_height.map(|window_height| {
                PlatformInput::MouseExited(MouseExitEvent {
                    position: point(
                        px(native_event.locationInWindow().x as f32),
                        window_height - px(native_event.locationInWindow().y as f32),
                    ),

                    pressed_button: None,
                    modifiers: read_modifiers(native_event),
                })
            }),
            _ => None,
        }
    }
}

/// The tablet's pen, from its proximity events: which end is in range, and where it was last.
#[derive(Clone, Copy, Default)]
struct Tablet {
    eraser: bool,
    last: Option<Point<Pixels>>,
}

thread_local! {
    static TABLET: Cell<Tablet> = Cell::new(Tablet::default());
}

/// A proximity event (or a mouse event of the proximity subtype): the pen leaving range leaves
/// the window.
unsafe fn note_proximity(native_event: &NSEvent) -> Option<PlatformInput> {
    let entering = native_event.isEnteringProximity();
    let eraser = native_event.pointingDeviceType() == NSPointingDeviceType::Eraser;
    let tablet = TABLET.with(|t| {
        let mut tablet = t.get();
        tablet.eraser = eraser;
        if !entering {
            tablet.last = None;
        }
        t.replace(tablet)
    });
    if entering {
        return None;
    }
    let position = tablet.last?;
    Some(PlatformInput::Pen(PenEvent {
        action: PenAction::Exited(MouseExitEvent {
            position,
            pressed_button: None,
            modifiers: read_modifiers(native_event),
        }),
        pen: PenInput {
            eraser,
            sample: PenSample {
                position,
                ..Default::default()
            },
            ..Default::default()
        },
    }))
}

/// A tablet reading: `tilt` is AppKit's, -1..1 per axis, and `rotation` its degrees.
pub(crate) fn pen_sample(
    position: Point<Pixels>,
    pressure: f32,
    tangential_pressure: f32,
    tilt: (f64, f64),
    rotation: f32,
) -> PenSample {
    PenSample {
        position,
        pressure: pressure.clamp(0.0, 1.0),
        tangential_pressure: tangential_pressure.clamp(-1.0, 1.0),
        tilt_x: (tilt.0.clamp(-1.0, 1.0) * 90.0) as f32,
        tilt_y: (tilt.1.clamp(-1.0, 1.0) * 90.0) as f32,
        twist: rotation.rem_euclid(360.0),
        width: 0.0,
        height: 0.0,
        time: None,
    }
}

/// When the hardware reported `event`: its `timestamp` and `systemUptime` share a clock.
fn event_time(event: &NSEvent) -> Option<Instant> {
    let age = NSProcessInfo::processInfo().systemUptime() - event.timestamp();
    Instant::now().checked_sub(Duration::from_secs_f64(age.max(0.0)))
}

/// `input` as a [`PenEvent`] when a tablet's pen made it (a mouse event of the tablet-point
/// subtype); otherwise as it is.
pub(crate) unsafe fn with_pen(native_event: *mut AnyObject, input: PlatformInput) -> PlatformInput {
    unsafe {
        let Some(native_event) = native_event.cast::<NSEvent>().as_ref() else {
            return input;
        };
        if !matches!(
            input,
            PlatformInput::MouseDown(_) | PlatformInput::MouseUp(_) | PlatformInput::MouseMove(_)
        ) {
            return input;
        }
        let subtype = native_event.subtype();
        if subtype == NSEventSubtype::TabletProximity {
            let _ = note_proximity(native_event);
            return input;
        }
        if subtype != NSEventSubtype::TabletPoint {
            return input;
        }
        let (position, contact, right) = match &input {
            PlatformInput::MouseDown(e) => (e.position, true, e.button == MouseButton::Right),
            PlatformInput::MouseUp(e) => (e.position, false, e.button == MouseButton::Right),
            PlatformInput::MouseMove(e) => (
                e.position,
                e.pressed_button.is_some(),
                e.pressed_button == Some(MouseButton::Right),
            ),
            _ => return input,
        };
        let tilt = native_event.tilt();
        let mut sample = pen_sample(
            position,
            if contact {
                native_event.pressure()
            } else {
                0.0
            },
            native_event.tangentialPressure(),
            (tilt.x, tilt.y),
            native_event.rotation(),
        );
        sample.time = event_time(native_event);
        let side = native_event.buttonMask().0
            & (NSEventButtonMask::PenLowerSide.0 | NSEventButtonMask::PenUpperSide.0)
            != 0;
        let eraser = TABLET.with(|t| {
            let mut tablet = t.get();
            tablet.last = Some(position);
            t.set(tablet);
            tablet.eraser
        });
        let action = match input {
            PlatformInput::MouseDown(e) => PenAction::Down(e),
            PlatformInput::MouseUp(e) => PenAction::Up(e),
            PlatformInput::MouseMove(e) => PenAction::Move(e),
            other => return other,
        };
        PlatformInput::Pen(PenEvent {
            action,
            pen: PenInput {
                pointer_id: 1,
                eraser,
                barrel: right || side,
                in_contact: contact,
                sample,
                history: Vec::new(),
            },
        })
    }
}

#[cfg(test)]
mod pen_tests {
    use super::*;

    #[test]
    fn appkit_tilt_and_rotation_become_degrees() {
        let s = pen_sample(point(px(1.0), px(2.0)), 1.5, -2.0, (0.5, -1.2), -30.0);
        assert_eq!(
            (
                s.pressure,
                s.tangential_pressure,
                s.tilt_x,
                s.tilt_y,
                s.twist
            ),
            (1.0, -1.0, 45.0, -90.0, 330.0)
        );
    }
}

fn parse_keystroke(native_event: &NSEvent) -> Keystroke {
    let characters = unsafe {
        let characters: *mut AnyObject = msg_send![native_event, charactersIgnoringModifiers];
        characters.to_str().to_string()
    };
    let mut key_char = None;
    let first_char = characters.chars().next().map(|ch| u32::from(ch as u16));
    let modifiers = native_event.modifierFlags();

    let control = modifiers.contains(NSEventModifierFlags::Control);
    let alt = modifiers.contains(NSEventModifierFlags::Option);
    let mut shift = modifiers.contains(NSEventModifierFlags::Shift);
    let command = modifiers.contains(NSEventModifierFlags::Command);
    let function = modifiers.contains(NSEventModifierFlags::Function)
        && first_char
            .is_none_or(|ch| !(NSUpArrowFunctionKey..=NSModeSwitchFunctionKey).contains(&ch));

    #[allow(non_upper_case_globals)]
    let key = match first_char {
        Some(SPACE_KEY) => {
            key_char = Some(" ".to_string());
            "space".to_string()
        }
        Some(TAB_KEY) => {
            key_char = Some("\t".to_string());
            "tab".to_string()
        }
        Some(ENTER_KEY) | Some(NUMPAD_ENTER_KEY) => {
            key_char = Some("\n".to_string());
            "enter".to_string()
        }
        Some(BACKSPACE_KEY) => "backspace".to_string(),
        Some(key) if key == u32::from(ESCAPE_KEY) => "escape".to_string(),
        Some(SHIFT_TAB_KEY) => "tab".to_string(),
        Some(NSUpArrowFunctionKey) => "up".to_string(),
        Some(NSDownArrowFunctionKey) => "down".to_string(),
        Some(NSLeftArrowFunctionKey) => "left".to_string(),
        Some(NSRightArrowFunctionKey) => "right".to_string(),
        Some(NSPageUpFunctionKey) => "pageup".to_string(),
        Some(NSPageDownFunctionKey) => "pagedown".to_string(),
        Some(NSHomeFunctionKey) => "home".to_string(),
        Some(NSEndFunctionKey) => "end".to_string(),
        Some(NSDeleteFunctionKey) => "delete".to_string(),
        // Observed Insert==NSHelpFunctionKey not NSInsertFunctionKey.
        Some(NSHelpFunctionKey) => "insert".to_string(),
        Some(NSF1FunctionKey) => "f1".to_string(),
        Some(NSF2FunctionKey) => "f2".to_string(),
        Some(NSF3FunctionKey) => "f3".to_string(),
        Some(NSF4FunctionKey) => "f4".to_string(),
        Some(NSF5FunctionKey) => "f5".to_string(),
        Some(NSF6FunctionKey) => "f6".to_string(),
        Some(NSF7FunctionKey) => "f7".to_string(),
        Some(NSF8FunctionKey) => "f8".to_string(),
        Some(NSF9FunctionKey) => "f9".to_string(),
        Some(NSF10FunctionKey) => "f10".to_string(),
        Some(NSF11FunctionKey) => "f11".to_string(),
        Some(NSF12FunctionKey) => "f12".to_string(),
        Some(NSF13FunctionKey) => "f13".to_string(),
        Some(NSF14FunctionKey) => "f14".to_string(),
        Some(NSF15FunctionKey) => "f15".to_string(),
        Some(NSF16FunctionKey) => "f16".to_string(),
        Some(NSF17FunctionKey) => "f17".to_string(),
        Some(NSF18FunctionKey) => "f18".to_string(),
        Some(NSF19FunctionKey) => "f19".to_string(),
        Some(NSF20FunctionKey) => "f20".to_string(),
        Some(NSF21FunctionKey) => "f21".to_string(),
        Some(NSF22FunctionKey) => "f22".to_string(),
        Some(NSF23FunctionKey) => "f23".to_string(),
        Some(NSF24FunctionKey) => "f24".to_string(),
        Some(NSF25FunctionKey) => "f25".to_string(),
        Some(NSF26FunctionKey) => "f26".to_string(),
        Some(NSF27FunctionKey) => "f27".to_string(),
        Some(NSF28FunctionKey) => "f28".to_string(),
        Some(NSF29FunctionKey) => "f29".to_string(),
        Some(NSF30FunctionKey) => "f30".to_string(),
        Some(NSF31FunctionKey) => "f31".to_string(),
        Some(NSF32FunctionKey) => "f32".to_string(),
        Some(NSF33FunctionKey) => "f33".to_string(),
        Some(NSF34FunctionKey) => "f34".to_string(),
        Some(NSF35FunctionKey) => "f35".to_string(),
        _ => {
            // Cases to test when modifying this:
            //
            //           qwerty key | none | cmd   | cmd-shift
            // * Armenian         s | ս    | cmd-s | cmd-shift-s  (layout is non-ASCII, so we use cmd layout)
            // * Dvorak+QWERTY    s | o    | cmd-s | cmd-shift-s  (layout switches on cmd)
            // * Ukrainian+QWERTY s | с    | cmd-s | cmd-shift-s  (macOS reports cmd-s instead of cmd-S)
            // * Czech            7 | ý    | cmd-ý | cmd-7        (layout has shifted numbers)
            // * Norwegian        7 | 7    | cmd-7 | cmd-/        (macOS reports cmd-shift-7 instead of cmd-/)
            // * Russian          7 | 7    | cmd-7 | cmd-&        (shift-7 is . but when cmd is down, should use cmd layout)
            // * German QWERTZ    ; | ö    | cmd-ö | cmd-Ö        (Zed's shift special case only applies to a-z)
            //
            let mut chars_ignoring_modifiers =
                chars_for_modified_key(native_event.keyCode(), NO_MOD);
            let mut chars_with_shift = chars_for_modified_key(native_event.keyCode(), SHIFT_MOD);
            let always_use_cmd_layout = always_use_command_layout();

            // Handle Dvorak+QWERTY / Russian / Armenian
            if command || always_use_cmd_layout {
                let chars_with_cmd = chars_for_modified_key(native_event.keyCode(), CMD_MOD);
                let chars_with_both =
                    chars_for_modified_key(native_event.keyCode(), CMD_MOD | SHIFT_MOD);

                // We don't do this in the case that the shifted command key generates
                // the same character as the unshifted command key (Norwegian, e.g.)
                if chars_with_both != chars_with_cmd {
                    chars_with_shift = chars_with_both;

                // Handle edge-case where cmd-shift-s reports cmd-s instead of
                // cmd-shift-s (Ukrainian, etc.)
                } else if chars_with_cmd.to_ascii_uppercase() != chars_with_cmd {
                    chars_with_shift = chars_with_cmd.to_ascii_uppercase();
                }
                chars_ignoring_modifiers = chars_with_cmd;
            }

            if !control && !command && !function {
                let mut mods = NO_MOD;
                if shift {
                    mods |= SHIFT_MOD;
                }
                if alt {
                    mods |= OPTION_MOD;
                }

                key_char = Some(chars_for_modified_key(native_event.keyCode(), mods));
            }

            if shift
                && chars_ignoring_modifiers
                    .chars()
                    .all(|c| c.is_ascii_lowercase())
            {
                chars_ignoring_modifiers
            } else if shift {
                shift = false;
                chars_with_shift
            } else {
                chars_ignoring_modifiers
            }
        }
    };

    Keystroke {
        modifiers: Modifiers {
            control,
            alt,
            shift,
            platform: command,
            function,
        },
        key,
        key_char,
    }
}

fn always_use_command_layout() -> bool {
    if chars_for_modified_key(0, NO_MOD).is_ascii() {
        return false;
    }

    chars_for_modified_key(0, CMD_MOD).is_ascii()
}

const NO_MOD: u32 = 0;
const CMD_MOD: u32 = 1;
const SHIFT_MOD: u32 = 2;
const OPTION_MOD: u32 = 8;

pub(crate) fn chars_for_modified_key(code: CGKeyCode, modifiers: u32) -> String {
    // Values from: https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/Headers/Events.h#L126
    // shifted >> 8 for UCKeyTranslate
    const CG_SPACE_KEY: u16 = 49;
    // https://github.com/phracker/MacOSX-SDKs/blob/master/MacOSX10.6.sdk/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/CarbonCore.framework/Versions/A/Headers/UnicodeUtilities.h#L278
    #[allow(non_upper_case_globals)]
    const kUCKeyActionDown: u16 = 0;
    #[allow(non_upper_case_globals)]
    const kUCKeyTranslateNoDeadKeysMask: u32 = 0;

    let keyboard_type = unsafe { LMGetKbdType() as u32 };
    const BUFFER_SIZE: usize = 4;
    let mut dead_key_state = 0;
    let mut buffer: [u16; BUFFER_SIZE] = [0; BUFFER_SIZE];
    let mut buffer_size: usize = 0;

    let keyboard = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
    if keyboard.is_null() {
        return "".to_string();
    }
    let layout_data = unsafe {
        TISGetInputSourceProperty(keyboard, kTISPropertyUnicodeKeyLayoutData as *const c_void)
            as CFDataRef
    };
    if layout_data.is_null() {
        unsafe {
            let _: () = msg_send![keyboard, release];
        }
        return "".to_string();
    }
    let keyboard_layout = unsafe { CFDataGetBytePtr(layout_data) };

    unsafe {
        UCKeyTranslate(
            keyboard_layout as *const c_void,
            code,
            kUCKeyActionDown,
            modifiers,
            keyboard_type,
            kUCKeyTranslateNoDeadKeysMask,
            &mut dead_key_state,
            BUFFER_SIZE,
            &mut buffer_size as *mut usize,
            &mut buffer as *mut u16,
        );
        if dead_key_state != 0 {
            UCKeyTranslate(
                keyboard_layout as *const c_void,
                CG_SPACE_KEY,
                kUCKeyActionDown,
                modifiers,
                keyboard_type,
                kUCKeyTranslateNoDeadKeysMask,
                &mut dead_key_state,
                BUFFER_SIZE,
                &mut buffer_size as *mut usize,
                &mut buffer as *mut u16,
            );
        }
        let _: () = msg_send![keyboard, release];
    }
    String::from_utf16(&buffer[..buffer_size]).unwrap_or_default()
}
