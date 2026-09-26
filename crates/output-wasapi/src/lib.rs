//! WASAPI exclusive-mode output backend.
//!
//! Bit-perfect: the device runs at the decoded stream's exact rate, channel
//! layout, and sample format. No resampling, remixing, or float/int
//! conversion is ever performed; formats the endpoint cannot run exactly are
//! rejected with `PlayerError::UnsupportedFormat` (never a closest-match
//! fallback).

mod candidates;

#[cfg(windows)]
mod imp;

#[cfg(windows)]
pub use imp::WasapiOutput;

#[cfg(not(windows))]
pub struct WasapiOutput {
    _private: (),
}

#[cfg(not(windows))]
impl WasapiOutput {
    pub fn new(_device: &sointty_core::DeviceId) -> Result<Self, sointty_core::PlayerError> {
        Err(sointty_core::PlayerError::InvalidInput(
            "WASAPI output is only available on Windows",
        ))
    }

    pub fn list_devices(
    ) -> Result<Vec<(sointty_core::DeviceId, String)>, sointty_core::PlayerError> {
        Err(sointty_core::PlayerError::InvalidInput(
            "WASAPI output is only available on Windows",
        ))
    }
}
