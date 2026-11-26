use gpui::{PlatformKeyboardLayout, SharedString};

#[derive(Clone)]
pub(crate) struct LinuxKeyboardLayout {
    name: SharedString,
    #[cfg(any(feature = "wayland", feature = "x11"))]
    base: Option<std::rc::Rc<xkbcommon::xkb::State>>,
}

impl PlatformKeyboardLayout for LinuxKeyboardLayout {
    fn id(&self) -> &str {
        &self.name
    }

    fn name(&self) -> &str {
        &self.name
    }

    #[cfg(any(feature = "wayland", feature = "x11"))]
    fn key_label(&self, code: gpui::NativeKeyCode) -> Option<String> {
        let gpui::NativeKeyCode::Evdev(key) = code else {
            return None;
        };
        Some(
            self.base
                .as_ref()?
                .key_get_utf8((u32::from(key) + 8).into()),
        )
    }
}

impl LinuxKeyboardLayout {
    pub(crate) fn new(name: SharedString) -> Self {
        Self {
            name,
            #[cfg(any(feature = "wayland", feature = "x11"))]
            base: None,
        }
    }

    #[cfg(any(feature = "wayland", feature = "x11"))]
    pub(crate) fn from_state(state: &xkbcommon::xkb::State) -> Self {
        use xkbcommon::xkb;
        let group = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        let map = state.get_keymap();
        let name = map.layout_get_name(group).to_owned().into();
        let mut base = xkb::State::new(&map);
        base.update_mask(0, 0, 0, 0, 0, group);
        Self {
            name,
            base: Some(std::rc::Rc::new(base)),
        }
    }
}
