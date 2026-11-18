//! vue-native: tells the embedder what happens to the renderer's GPU device.

use std::sync::OnceLock;

/// What happened to the renderer's GPU device. Reported on the UI thread.
#[derive(Clone, Copy, Debug)]
pub enum DeviceLoss<'a> {
    /// The device was removed (a driver reset, update or crash, or the GPU went away), or the
    /// renderer asked for new devices. Everything made on it must be released: D3D12 makes no
    /// new device for the adapter while anything still references the removed one.
    Removed { reason: &'a str },
    /// The renderer draws on a new device.
    Recovered,
    /// No new device could be made; the app is asked to quit.
    Failed { error: &'a str },
}

static OBSERVER: OnceLock<fn(DeviceLoss<'_>)> = OnceLock::new();

/// Installs the hook that hears [`DeviceLoss`], before the platform starts.
pub fn install_device_loss_observer(observer: fn(DeviceLoss<'_>)) -> anyhow::Result<()> {
    OBSERVER
        .set(observer)
        .map_err(|_| anyhow::anyhow!("device loss observer already installed"))
}

pub(crate) fn report(event: DeviceLoss<'_>) {
    match OBSERVER.get() {
        Some(observer) => observer(event),
        None => match event {
            DeviceLoss::Removed { reason } => log::error!("the GPU device was removed: {reason}"),
            DeviceLoss::Recovered => log::info!("the GPU device was recreated"),
            DeviceLoss::Failed { error } => {
                log::error!("the GPU device was lost and could not be recreated: {error}")
            }
        },
    }
}
