//! WASAPI output backend: exact exclusive Direct by default, OS-mixed Shared
//! only under the explicit `shared_playback` opt-in.
//!
//! Direct is bit-perfect when the endpoint accepts the decoded stream's exact
//! rate, layout and sample format. Shared submits source-rate/layout client
//! PCM to the WASAPI audio engine, which may convert, resample, mix or apply
//! volume processing; Shared is never bit-perfect and never falls back to
//! Direct or to a closest-match client descriptor.

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
    pub fn new(
        _device: &sointty_core::DeviceId,
        _access: sointty_core::OutputAccess,
    ) -> Result<Self, sointty_core::PlayerError> {
        Err(sointty_core::PlayerError::InvalidInput(
            "WASAPI output is only available on Windows",
        ))
    }

    pub fn list_devices() -> Result<Vec<(sointty_core::DeviceId, String)>, sointty_core::PlayerError>
    {
        Err(sointty_core::PlayerError::InvalidInput(
            "WASAPI output is only available on Windows",
        ))
    }
}
