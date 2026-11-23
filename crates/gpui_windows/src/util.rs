use std::sync::OnceLock;

use anyhow::Context;
use gpui_util::ResultExt;
use windows::{
    UI::{
        Color,
        ViewManagement::{UIColorType, UISettings},
    },
    Win32::{
        Foundation::*, Graphics::Dwm::*, System::LibraryLoader::LoadLibraryA,
        UI::WindowsAndMessaging::*,
    },
    core::{BOOL, PCSTR},
};

use crate::*;
use gpui::*;

pub(crate) trait HiLoWord {
    fn hiword(&self) -> u16;
    fn loword(&self) -> u16;
    fn signed_hiword(&self) -> i16;
    fn signed_loword(&self) -> i16;
}

impl HiLoWord for WPARAM {
    fn hiword(&self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }

    fn loword(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    fn signed_hiword(&self) -> i16 {
        ((self.0 >> 16) & 0xFFFF) as i16
    }

    fn signed_loword(&self) -> i16 {
        (self.0 & 0xFFFF) as i16
    }
}

impl HiLoWord for LPARAM {
    fn hiword(&self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }

    fn loword(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    fn signed_hiword(&self) -> i16 {
        ((self.0 >> 16) & 0xFFFF) as i16
    }

    fn signed_loword(&self) -> i16 {
        (self.0 & 0xFFFF) as i16
    }
}

pub(crate) unsafe fn get_window_long(hwnd: HWND, nindex: WINDOW_LONG_PTR_INDEX) -> isize {
    #[cfg(target_pointer_width = "64")]
    unsafe {
        GetWindowLongPtrW(hwnd, nindex)
    }
    #[cfg(target_pointer_width = "32")]
    unsafe {
        GetWindowLongW(hwnd, nindex) as isize
    }
}

pub(crate) unsafe fn set_window_long(
    hwnd: HWND,
    nindex: WINDOW_LONG_PTR_INDEX,
    dwnewlong: isize,
) -> isize {
    #[cfg(target_pointer_width = "64")]
    unsafe {
        SetWindowLongPtrW(hwnd, nindex, dwnewlong)
    }
    #[cfg(target_pointer_width = "32")]
    unsafe {
        SetWindowLongW(hwnd, nindex, dwnewlong as i32) as isize
    }
}

/// A Win32 call whose zero return is not always a failure (a timer already gone, a declined
/// foreground request), checked by the error it set rather than the return alone.
pub(crate) fn win32_call(
    call: impl FnOnce() -> windows::core::Result<()>,
) -> windows::core::Result<()> {
    unsafe { SetLastError(ERROR_SUCCESS) };
    match call() {
        Err(error) if error.code().is_ok() => Ok(()),
        result => result,
    }
}

/// Invalidates `hwnd` from another thread. A window destroyed on its own thread stays listed
/// until that thread drops it, so a handle gone meanwhile is not an error.
pub(crate) fn redraw_window(hwnd: HWND) -> windows::core::Result<()> {
    use windows::Win32::Graphics::Gdi::{RDW_INVALIDATE, RedrawWindow};
    match unsafe { RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE) }.ok() {
        Err(error) if error.code() == ERROR_INVALID_WINDOW_HANDLE.to_hresult() => Ok(()),
        result => result,
    }
}

pub(crate) fn windows_credentials_target_name(url: &str) -> String {
    format!("zed:url={}", url)
}

pub(crate) fn load_cursor(style: CursorStyle) -> Option<HCURSOR> {
    static ARROW: OnceLock<SafeCursor> = OnceLock::new();
    static IBEAM: OnceLock<SafeCursor> = OnceLock::new();
    static CROSS: OnceLock<SafeCursor> = OnceLock::new();
    static HAND: OnceLock<SafeCursor> = OnceLock::new();
    static SIZEWE: OnceLock<SafeCursor> = OnceLock::new();
    static SIZENS: OnceLock<SafeCursor> = OnceLock::new();
    static SIZENWSE: OnceLock<SafeCursor> = OnceLock::new();
    static SIZENESW: OnceLock<SafeCursor> = OnceLock::new();
    static NO: OnceLock<SafeCursor> = OnceLock::new();
    static HELP: OnceLock<SafeCursor> = OnceLock::new();
    static APPSTARTING: OnceLock<SafeCursor> = OnceLock::new();
    static WAIT: OnceLock<SafeCursor> = OnceLock::new();
    static SIZEALL: OnceLock<SafeCursor> = OnceLock::new();
    let (lock, name) = match style {
        CursorStyle::Hidden => return None,
        CursorStyle::Help => (&HELP, IDC_HELP),
        CursorStyle::Progress => (&APPSTARTING, IDC_APPSTARTING),
        CursorStyle::Wait => (&WAIT, IDC_WAIT),
        CursorStyle::Move | CursorStyle::AllScroll => (&SIZEALL, IDC_SIZEALL),
        CursorStyle::Cell => (&CROSS, IDC_CROSS),
        CursorStyle::NoDrop => (&NO, IDC_NO),
        CursorStyle::IBeam | CursorStyle::IBeamCursorForVerticalLayout => (&IBEAM, IDC_IBEAM),
        CursorStyle::Crosshair => (&CROSS, IDC_CROSS),
        CursorStyle::PointingHand | CursorStyle::DragLink => (&HAND, IDC_HAND),
        CursorStyle::ResizeLeft
        | CursorStyle::ResizeRight
        | CursorStyle::ResizeLeftRight
        | CursorStyle::ResizeColumn => (&SIZEWE, IDC_SIZEWE),
        CursorStyle::ResizeUp
        | CursorStyle::ResizeDown
        | CursorStyle::ResizeUpDown
        | CursorStyle::ResizeRow => (&SIZENS, IDC_SIZENS),
        CursorStyle::ResizeUpLeftDownRight => (&SIZENWSE, IDC_SIZENWSE),
        CursorStyle::ResizeUpRightDownLeft => (&SIZENESW, IDC_SIZENESW),
        CursorStyle::OperationNotAllowed => (&NO, IDC_NO),
        _ => (&ARROW, IDC_ARROW),
    };
    Some(
        *(*lock.get_or_init(|| {
            HCURSOR(
                unsafe { LoadImageW(None, name, IMAGE_CURSOR, 0, 0, LR_DEFAULTSIZE | LR_SHARED) }
                    .log_err()
                    .unwrap_or_default()
                    .0,
            )
            .into()
        })),
    )
}

/// This function is used to configure the dark mode for the window built-in title bar.
pub(crate) fn configure_dwm_dark_mode(hwnd: HWND, appearance: WindowAppearance) {
    let dark_mode_enabled: BOOL = match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => true.into(),
        WindowAppearance::Light | WindowAppearance::VibrantLight => false.into(),
    };
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_mode_enabled as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        )
        .log_err();
    }
}

#[inline]
pub(crate) fn logical_point(x: f32, y: f32, scale_factor: f32) -> Point<Pixels> {
    Point {
        x: px(x / scale_factor),
        y: px(y / scale_factor),
    }
}

/// vue-native fork: the scale factor for a DPI, or `GPUI_SCALE_FACTOR` in its place, which tests
/// use to run at a fractional scale on any monitor.
pub(crate) fn dpi_scale_factor(dpi: f32) -> f32 {
    static FORCED: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    FORCED
        .get_or_init(|| {
            std::env::var("GPUI_SCALE_FACTOR")
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|s| s.is_finite() && *s >= 0.5 && *s <= 4.0)
        })
        .unwrap_or(dpi / windows::Win32::UI::WindowsAndMessaging::USER_DEFAULT_SCREEN_DPI as f32)
}

// https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/apply-windows-themes
#[inline]
pub(crate) fn system_appearance() -> Result<WindowAppearance> {
    let ui_settings = UISettings::new()?;
    let foreground_color = ui_settings.GetColorValue(UIColorType::Foreground)?;
    // If the foreground is light, then is_color_light will evaluate to true,
    // meaning Dark mode is enabled.
    if is_color_light(&foreground_color) {
        Ok(WindowAppearance::Dark)
    } else {
        Ok(WindowAppearance::Light)
    }
}

#[inline(always)]
fn is_color_light(color: &Color) -> bool {
    ((5 * color.G as u32) + (2 * color.R as u32) + color.B as u32) > (8 * 128)
}

pub(crate) fn with_dll_library<R, F>(dll_name: PCSTR, f: F) -> Result<R>
where
    F: FnOnce(HMODULE) -> Result<R>,
{
    let library = unsafe {
        LoadLibraryA(dll_name).with_context(|| format!("Loading dll: {}", dll_name.display()))?
    };
    let result = f(library);
    unsafe {
        FreeLibrary(library)
            .with_context(|| format!("Freeing dll: {}", dll_name.display()))
            .log_err();
    }
    result
}

#[cfg(test)]
mod tests {
    use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};
    use windows::core::w;

    use super::{redraw_window, win32_call};

    fn hidden_window() -> HWND {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                None,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a window")
    }

    #[test]
    fn a_zero_return_without_an_error_and_redrawing_a_destroyed_window_are_not_failures() {
        let hwnd = hidden_window();
        let kill = || win32_call(|| unsafe { KillTimer(Some(hwnd), 1) });
        assert!(kill().is_ok());
        unsafe { DestroyWindow(hwnd) }.expect("destroyed");
        assert!(redraw_window(hwnd).is_ok());
        assert!(kill().is_err());
    }
}
