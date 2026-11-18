//! vue-native: how a layer asks for EDR. From macOS 26 and iOS 26 it sets `preferredDynamicRange`
//! and tags `contentsHeadroom` with what the content needs, so the system raises the screen's
//! headroom only that far; with `wantsExtendedDynamicRangeContent`, the only way before, it raises
//! it to the panel's maximum.

use std::ffi::CStr;

use objc2::runtime::NSObjectProtocol as _;
use objc2::sel;
use objc2_core_foundation::CGFloat;
use objc2_quartz_core::{CADynamicRange, CAMetalLayer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EdrApi {
    DynamicRange,
    Legacy,
}

impl EdrApi {
    pub(super) fn of(layer: &CAMetalLayer) -> Self {
        let available = layer.respondsToSelector(sel!(setPreferredDynamicRange:))
            && layer.respondsToSelector(sel!(setContentsHeadroom:))
            && dynamic_range(true).is_some()
            && dynamic_range(false).is_some();
        if available {
            Self::DynamicRange
        } else {
            Self::Legacy
        }
    }
}

/// Asks for EDR up to `headroom`, in units of SDR white, or for standard range with `None`.
pub(super) fn request(layer: &CAMetalLayer, headroom: Option<f32>, api: EdrApi) {
    match (api, dynamic_range(headroom.is_some())) {
        (EdrApi::DynamicRange, Some(range)) => {
            // 0 is untagged, and values between 0 and 1.0 are undefined.
            layer.setContentsHeadroom(headroom.map_or(0.0, |h| CGFloat::from(h.max(1.0))));
            layer.setPreferredDynamicRange(range);
        }
        _ => layer.setWantsExtendedDynamicRangeContent(headroom.is_some()),
    }
}

/// `CADynamicRangeHigh` or `CADynamicRangeStandard`, looked up at run time: QuartzCore exports
/// them from macOS 26, and linking them would keep the app from loading on earlier versions.
pub(super) fn dynamic_range(high: bool) -> Option<&'static CADynamicRange> {
    let name: &CStr = if high {
        c"CADynamicRangeHigh"
    } else {
        c"CADynamicRangeStandard"
    };
    // SAFETY: `dlsym` with `RTLD_DEFAULT` and a NUL-terminated name only searches loaded images.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    // SAFETY: where QuartzCore defines the symbol it is a `CADynamicRange const`, a pointer to a
    // constant string that lives as long as the process.
    unsafe { symbol.cast::<*const CADynamicRange>().as_ref()?.as_ref() }
}
