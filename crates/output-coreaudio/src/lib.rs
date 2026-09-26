//! macOS CoreAudio backend for Sointty.
//!
//! The whole HAL backend is `cfg(target_os = "macos")`; on other hosts only
//! the platform-independent physical-format matching logic is compiled (so it
//! can be unit-tested anywhere).

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod format;

#[cfg(target_os = "macos")]
mod hal;

#[cfg(target_os = "macos")]
pub use hal::CoreAudioOutput;
