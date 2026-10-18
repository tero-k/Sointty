//! macOS Core Audio backends for Sointty: unchanged Direct HAL hog mode and
//! separate Shared AUHAL output under the explicit `shared_playback` opt-in.
//!
//! The platform backends are `cfg(target_os = "macos")`; on other hosts only
//! the platform-independent physical-format matching logic is compiled.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod format;

#[cfg(target_os = "macos")]
mod hal;

#[cfg(target_os = "macos")]
mod shared;

#[cfg(target_os = "macos")]
pub use hal::CoreAudioOutput;

#[cfg(target_os = "macos")]
pub use shared::SharedCoreAudioOutput;
