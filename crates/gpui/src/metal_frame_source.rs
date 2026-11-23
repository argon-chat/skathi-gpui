//! vue-native fork: where a macOS window's frames get their Metal drawables.

use std::sync::atomic::{AtomicU8, Ordering};

/// The drawables a macOS window draws into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetalFrameSource {
    /// `CAMetalDisplayLink` paces frames (macOS 14+); a resize, an activation, a surface frame
    /// after a pause, a covered window and a stalled link draw with `nextDrawable` instead.
    #[default]
    Auto,
    /// Only `CAMetalDisplayLink`: those frames wait for the link's next update, and a covered
    /// window does not draw. Falls back to `nextDrawable` where the link does not exist.
    DisplayLink,
    /// Only `nextDrawable`, paced by `CVDisplayLink`.
    NextDrawable,
}

/// Why a window is about to draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOccasion {
    /// The regular paced frame.
    Paced,
    /// A frame wanted at once: a resize, an activation, a surface frame after a pause, a view
    /// moved to another window.
    Immediate,
    /// The window is covered and asked to keep drawing.
    Covered,
    /// The display link stopped delivering.
    Stalled,
}

/// How that frame gets its drawable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramePath {
    /// From the `CAMetalDisplayLink` update.
    DisplayLink,
    /// From the layer's `nextDrawable`, now or paced by `CVDisplayLink`.
    NextDrawable,
    /// Left to the display link's next update, which is woken.
    WakeDisplayLink,
}

impl MetalFrameSource {
    /// The path a frame takes for `occasion`; `link_available` is whether this system has
    /// `CAMetalDisplayLink`.
    pub fn path(self, occasion: FrameOccasion, link_available: bool) -> FramePath {
        if !link_available {
            return FramePath::NextDrawable;
        }
        match (self, occasion) {
            (Self::NextDrawable, _) => FramePath::NextDrawable,
            (Self::Auto, FrameOccasion::Paced) => FramePath::DisplayLink,
            (Self::Auto, _) => FramePath::NextDrawable,
            (Self::DisplayLink, FrameOccasion::Immediate) => FramePath::WakeDisplayLink,
            (Self::DisplayLink, _) => FramePath::DisplayLink,
        }
    }
}

static SOURCE: AtomicU8 = AtomicU8::new(0);

/// macOS: the frame source of windows opened after this call.
pub fn set_metal_frame_source(source: MetalFrameSource) {
    let value = match source {
        MetalFrameSource::Auto => 0,
        MetalFrameSource::DisplayLink => 1,
        MetalFrameSource::NextDrawable => 2,
    };
    SOURCE.store(value, Ordering::Release);
}

/// What [`set_metal_frame_source`] set.
pub fn metal_frame_source() -> MetalFrameSource {
    match SOURCE.load(Ordering::Acquire) {
        1 => MetalFrameSource::DisplayLink,
        2 => MetalFrameSource::NextDrawable,
        _ => MetalFrameSource::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_source_never_takes_the_other_path() {
        use FrameOccasion::*;
        use FramePath::*;
        let all = [Paced, Immediate, Covered, Stalled];
        let paths =
            |source: MetalFrameSource, link| all.map(|occasion| source.path(occasion, link));
        assert_eq!(
            paths(MetalFrameSource::Auto, true),
            [DisplayLink, NextDrawable, NextDrawable, NextDrawable]
        );
        assert_eq!(
            paths(MetalFrameSource::DisplayLink, true),
            [DisplayLink, WakeDisplayLink, DisplayLink, DisplayLink]
        );
        assert_eq!(
            paths(MetalFrameSource::NextDrawable, true),
            [NextDrawable; 4]
        );
        for source in [MetalFrameSource::Auto, MetalFrameSource::DisplayLink] {
            assert_eq!(
                paths(source, false),
                [NextDrawable; 4],
                "{source:?} without the link"
            );
        }
    }
}
