#![cfg(target_os = "windows")]

mod clipboard;
mod destination_list;
mod device_loss;
mod dialog;
mod direct_manipulation;
mod direct_write;
#[cfg(feature = "native-dx12")]
mod directx12;
#[cfg_attr(feature = "native-dx12", allow(dead_code))]
mod directx_atlas;
#[cfg_attr(feature = "native-dx12", allow(dead_code))]
mod directx_devices;
#[cfg_attr(feature = "native-dx12", allow(dead_code))]
mod directx_renderer;
mod dispatcher;
mod display;
mod drag_source;
mod events;
mod frame_pacing;
mod haptics;
mod keyboard;
mod pen;
mod platform;
mod pointer_lock;
mod present_feedback;
mod profiling;
mod system_notifications;
mod system_settings;
mod util;
mod vsync;
mod window;
mod wrapper;

pub(crate) use clipboard::*;
pub(crate) use destination_list::*;
pub(crate) use direct_write::*;
#[cfg(feature = "native-dx12")]
pub(crate) use directx_devices::try_to_recover_from_device_lost;
#[cfg(not(feature = "native-dx12"))]
pub(crate) use directx_devices::*;
#[cfg(feature = "native-dx12")]
pub(crate) use directx_renderer::DISABLE_DIRECT_COMPOSITION;
#[cfg(not(feature = "native-dx12"))]
pub(crate) use directx_renderer::*;
#[cfg(feature = "native-dx12")]
pub(crate) use directx12::{DirectXDevices, DirectXRenderer};
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use events::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use system_notifications::*;
pub(crate) use system_settings::*;
pub(crate) use util::*;
pub(crate) use vsync::*;
pub(crate) use window::*;
pub(crate) use wrapper::*;

pub use device_loss::{DeviceLoss, install_device_loss_observer};
#[cfg(not(feature = "native-dx12"))]
pub use directx_devices::renderer_d3d11_device;
/// vue-native: the renderer's D3D11 device, so `<video>` can hand it to Media
/// Foundation and have decoded frames land in VRAM the renderer already owns.
/// See `directx_devices.rs`.
pub use directx_devices::{ExternalDirectXDevice, install_directx_device_factory};
#[cfg(feature = "native-dx12")]
pub use directx12::{RendererD3D11Handle, ensure_renderer_d3d11_device, renderer_d3d11_device};
#[cfg(feature = "native-dx12")]
pub use directx12::{
    RendererD3D12Handle, install_renderer_d3d12_generation_observer, renderer_d3d12_device,
};
pub use frame_pacing::{pace_frames, pace_frames_from_frame, wake_frames};
pub use platform::WindowsPlatform;
pub use profiling::{FrameProfilerCallbacks, set_frame_profiler};

pub(crate) use windows::Win32::Foundation::HWND;
