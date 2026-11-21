// Keep the existing Windows API while sharing the optional hooks with GPUI.
#[cfg(feature = "native-dx12")]
pub(crate) use gpui::native_profiling::span;
pub use gpui::native_profiling::{FrameProfilerCallbacks, set_frame_profiler};
