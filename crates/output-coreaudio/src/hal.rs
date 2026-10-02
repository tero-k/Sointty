//! macOS CoreAudio HAL backend (`sointty-output-coreaudio`).
//!
//! Bit-perfect exclusive access is achieved through the HAL's hog mode rather
//! than a mode switch: while we hold `kAudioDevicePropertyHogMode` no other
//! client (including the system mixer) can use the device, and the stream's
//! *physical* format is set to the exact decoded rate/channels/encoding — no
//! resampling, remixing, or sample conversion anywhere.
//!
//! # Unsafe FFI containment
//!
//! Every CoreAudio/CoreFoundation call in this file is `unsafe` FFI. The
//! unsafe surface is contained in three places:
//!
//! - [`get_data`] / [`get_value`] / [`set_value`]: thin typed wrappers over
//!   `AudioObjectGetPropertyData(Size)` / `AudioObjectSetPropertyData`. Safety
//!   invariant: the caller must request the property with the exact data type
//!   `T` the HAL documents for that selector/scope/element.
//! - [`CoreAudioOutput::start`]/[`CoreAudioOutput::stop`]: device lifetime
//!   calls (`AudioDeviceCreateIOProcIDWithBlock`, `AudioDeviceStart/Stop`,
//!   `AudioDeviceDestroyIOProcID`) with documented preconditions.
//! - [`io_proc`]: the real-time render callback invoked by the HAL on its own
//!   thread. It only touches memory the HAL hands it plus the lock-free rtrb
//!   consumer and atomics; see its doc comment.
//!
//! The two `extern` declarations at the bottom are the only direct
//! CoreFoundation entry points (copying/release of the `CFStringRef` returned
//! by `kAudioObjectPropertyName`); everything else comes from the audited
//! `objc2-core-audio` bindings.

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use block2::RcBlock;
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    AudioDeviceIOProcID, kAudioDevicePropertyActualSampleRate,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyHogMode,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreams,
    kAudioHardwareNoError, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyAvailablePhysicalFormats, kAudioStreamPropertyPhysicalFormat,
    AudioStreamRangedDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use sointty_core::{
    AudioOutput, BufferConfig, DeviceId, OutputCounters, OutputSpec, PlayerError, StreamSpec,
};

use crate::format::{CandidateFormat, pick_physical_format};

/// The HAL block signature: (now, input_data, input_time, output_data,
/// output_time); we only write `output_data`.
type IoBlockDyn = dyn Fn(
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
);

const LOGGED_BUFFERS: usize = 8;

/// One render thread's best-effort diagnostics. The HAL writes only atomics;
/// `stop` formats and writes the log after the IO block has been destroyed.
struct RenderReport {
    first_xrun_frame: AtomicU64,
    promotion_failed: AtomicBool,
    buffer_count: AtomicU32,
    buffer_bytes: [AtomicU32; LOGGED_BUFFERS],
    frame_bytes: u32,
}

impl RenderReport {
    fn new(frame_bytes: u32) -> Self {
        Self {
            first_xrun_frame: AtomicU64::new(u64::MAX),
            promotion_failed: AtomicBool::new(false),
            buffer_count: AtomicU32::new(0),
            buffer_bytes: std::array::from_fn(|_| AtomicU32::new(0)),
            frame_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// Property helpers (all unsafe FFI contained here)
// ---------------------------------------------------------------------------

/// Open the opt-in diagnostic log (`SOINTTY_COREAUDIO_LOG` env var) for
/// format-negotiation forensics. Never called on the HAL render thread.
fn debug_log() -> Option<std::fs::File> {
    let path = std::env::var_os("SOINTTY_COREAUDIO_LOG")?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn prop_addr(
    selector: u32,
    scope: u32,
    element: u32,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

/// Read a variable-length property into a `Vec<T>`.
///
/// # Safety
///
/// `T` must be exactly the data type the HAL returns for `address` on
/// `object` (or an array element of it). `address` must be valid for
/// `object`.
fn get_data<T: Copy>(object: AudioObjectID, address: &AudioObjectPropertyAddress) -> Result<Vec<T>, i32> {
    // SAFETY: `address` and `size` are valid, aligned stack references; the
    // HAL only reads/writes through them for the duration of the call.
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(address),
            0,
            std::ptr::null(),
            NonNull::new(&mut size).expect("non-null out-size reference"),
        )
    };
    if status != kAudioHardwareNoError {
        return Err(status);
    }
    let count = size as usize / std::mem::size_of::<T>();
    if count == 0 {
        return Ok(Vec::new());
    }
    // Allocation happens only on the configuration path; the RT render
    // callback never comes through here.
    let mut buf: Vec<T> = Vec::with_capacity(count);
    // SAFETY: `buf` has room for `count` elements; the HAL writes at most
    // `size` bytes. `out_data` is a valid non-null pointer into `buf`.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(address),
            0,
            std::ptr::null(),
            NonNull::new(&mut size).expect("non-null io-size reference"),
            NonNull::new(buf.as_mut_ptr().cast::<c_void>()).expect("non-null data buffer"),
        )
    };
    if status != kAudioHardwareNoError {
        return Err(status);
    }
    // SAFETY: the HAL reported success, so `size` bytes (>= count * sizeof(T)
    // elements, or the property shrank) are initialized. Clamp to the
    // property-reported element count.
    let returned = (size as usize / std::mem::size_of::<T>()).min(count);
    // SAFETY: `returned <= capacity` elements are initialized by the HAL.
    unsafe { buf.set_len(returned) };
    Ok(buf)
}

/// Read a single-value property.
///
/// # Safety
///
/// Same contract as [`get_data`].
fn get_value<T: Copy>(object: AudioObjectID, address: &AudioObjectPropertyAddress) -> Result<T, i32> {
    get_data(object, address)?
        .into_iter()
        .next()
        .ok_or(-1) // kAudioHardwareNoError-adjacent sentinel: property empty
}

/// Write a single-value property.
///
/// # Safety
///
/// `T` must be exactly the data type the HAL expects for `address` on
/// `object`.
fn set_value<T: Copy>(object: AudioObjectID, address: &AudioObjectPropertyAddress, value: &T) -> Result<(), i32> {
    // SAFETY: `value` points to one initialized `T`; the HAL reads
    // `size_of::<T>()` bytes during the call.
    let status = unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(address),
            0,
            std::ptr::null(),
            std::mem::size_of::<T>() as u32,
            NonNull::from(value).cast::<c_void>(),
        )
    };
    if status != kAudioHardwareNoError {
        return Err(status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal contained CoreFoundation FFI (device-name CFString only)
// ---------------------------------------------------------------------------

unsafe extern "C" {
    /// Copy a `CFStringRef` into a UTF-8 C buffer. Returns false if it does
    /// not fit; the caller must then fall back to `CFStringGetCStringP` style
    /// retry, which we deliberately skip (device names fit in 256 bytes).
    fn CFStringGetCString(
        the_string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    /// Balance the `+1` retain the HAL's `GetPropertyData` returns for
    /// `kAudioObjectPropertyName`.
    fn CFRelease(cf: *const c_void);
}

/// `kCFStringEncodingUTF8`
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

fn device_name(object: AudioObjectID) -> Option<String> {
    let name_ref: *mut c_void =
        get_value(object, &prop_addr(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain))
            .ok()?;
    if name_ref.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    // SAFETY: `name_ref` is a live CFStringRef from the HAL; `buf` is a valid
    // 256-byte output buffer. We release the ref immediately afterwards.
    let ok = unsafe {
        CFStringGetCString(
            name_ref,
            buf.as_mut_ptr(),
            buf.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    // SAFETY: `name_ref` is the retained string we just copied from.
    unsafe { CFRelease(name_ref) };
    if !ok {
        return None;
    }
    // SAFETY: on success the buffer holds a NUL-terminated UTF-8 string.
    let cstr = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(cstr.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// CoreAudioOutput
// ---------------------------------------------------------------------------

/// CoreAudio (HAL) bit-perfect output device.
///
/// `Send` is implemented manually: the only non-`Send` field is the retained
/// `RcBlock`, which we only create, pass to the HAL, and release from the
/// thread owning `self` (in `stop`). The block's *captured* state (rtrb
/// consumer, `Arc<OutputCounters>`, `Cell`s, integers) is all safe to use
/// from the HAL's render thread, which is where the copy runs.
pub struct CoreAudioOutput {
    device: AudioObjectID,
    device_id: DeviceId,
    io_proc: AudioDeviceIOProcID,
    block: Option<RcBlock<IoBlockDyn>>,
    hog_acquired: bool,
    spec: Option<OutputSpec>,
    /// Retained handle to the render counters so `stop` can record the final
    /// played/xrun totals in the diagnostic log. Not read on the RT path.
    counters: Option<Arc<OutputCounters>>,
    /// Retained until `stop` can read it after the callback is destroyed.
    report: Option<Arc<RenderReport>>,
}

// SAFETY: see the struct documentation. The HAL retains its own copy of the
// block; our `RcBlock` handle is only touched from the owning thread.
unsafe impl Send for CoreAudioOutput {}

impl CoreAudioOutput {
    /// `device`: `"default"` for the default render endpoint, or the decimal
    /// `AudioObjectID` of a render endpoint (see [`CoreAudioOutput::list_devices`]).
    pub fn new(device: &DeviceId) -> Result<Self, PlayerError> {
        let (object, device_id) = if device == "default" {
            // SAFETY: system object + default-output selector is a documented
            // global-scope read returning one AudioObjectID.
            let object: AudioObjectID = get_value(
                kAudioObjectSystemObject as AudioObjectID,
                &prop_addr(
                    kAudioHardwarePropertyDefaultOutputDevice,
                    kAudioObjectPropertyScopeGlobal,
                    kAudioObjectPropertyElementMain,
                ),
            )
            .map_err(|_| PlayerError::DeviceLost)?;
            if object == 0 {
                return Err(PlayerError::DeviceLost);
            }
            (object, object.to_string())
        } else {
            let object: AudioObjectID = device.parse().map_err(|_| {
                PlayerError::InvalidInput(
                    "device id must be \"default\" or a decimal AudioObjectID",
                )
            })?;
            (object, device.clone())
        };
        Ok(Self {
            device: object,
            device_id,
            io_proc: None,
            block: None,
            hog_acquired: false,
            spec: None,
            counters: None,
            report: None,
        })
    }

    /// `(endpoint id, friendly name)` for all active output-capable render
    /// endpoints.
    pub fn list_devices() -> Result<Vec<(DeviceId, String)>, PlayerError> {
        // SAFETY: system-object global-scope read returning an array of
        // AudioObjectID.
        let devices: Vec<AudioObjectID> = get_data(
            kAudioObjectSystemObject as AudioObjectID,
            &prop_addr(
                kAudioHardwarePropertyDevices,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map_err(|_| PlayerError::DeviceLost)?;

        let mut out = Vec::new();
        for device in devices {
            // Output-capable == has at least one stream in the output scope.
            // SAFETY: documented read returning an array of AudioObjectID.
            let output_streams: Vec<AudioObjectID> = get_data(
                device,
                &prop_addr(
                    kAudioDevicePropertyStreams,
                    kAudioObjectPropertyScopeOutput,
                    kAudioObjectPropertyElementMain,
                ),
            )
            .unwrap_or_default();
            if output_streams.is_empty() {
                continue;
            }
            let name = device_name(device).unwrap_or_else(|| device.to_string());
            out.push((device.to_string(), name));
        }
        Ok(out)
    }

    fn unsupported(input: &StreamSpec, reason: &'static str) -> PlayerError {
        PlayerError::UnsupportedFormat {
            rate_hz: input.rate_hz,
            channels: input.layout.channels,
            encoding: input.encoding,
            reason,
        }
    }

    /// Take hog mode (exclusive access). Any failure maps to
    /// [`PlayerError::DeviceBusy`]: the device is either already hogged by
    /// another process or does not support exclusive access.
    fn acquire_hog(&mut self) -> Result<(), PlayerError> {
        let address = prop_addr(
            kAudioDevicePropertyHogMode,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: hog mode is a documented pid_t property on audio devices.
        let current: i32 = get_value(self.device, &address).map_err(|_| PlayerError::DeviceBusy)?;
        if current != -1 {
            return Err(PlayerError::DeviceBusy);
        }
        let pid = std::process::id() as i32;
        // SAFETY: setting our own pid on the hog-mode property requests
        // exclusive access.
        set_value(self.device, &address, &pid).map_err(|_| PlayerError::DeviceBusy)?;
        self.hog_acquired = true;
        Ok(())
    }

    /// Release hog mode if (and only if) we still hold it. Best-effort and
    /// never panics; used on every failure path after `acquire_hog`.
    fn release_hog(&mut self) {
        if !self.hog_acquired {
            return;
        }
        let address = prop_addr(
            kAudioDevicePropertyHogMode,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: documented pid_t property.
        if let Ok(current) = get_value::<i32>(self.device, &address) {
            if current == std::process::id() as i32 {
                let none: i32 = -1;
                // SAFETY: -1 releases the device for other clients.
                let _ = set_value(self.device, &address, &none);
            }
        }
        self.hog_acquired = false;
    }

    fn configure_inner(
        &mut self,
        input: &StreamSpec,
        buffers: BufferConfig,
    ) -> Result<OutputSpec, PlayerError> {
        // (b) Exact nominal sample rate.
        let rate_address = prop_addr(
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        let wanted_rate = f64::from(input.rate_hz);
        // SAFETY: documented Float64 property.
        set_value(self.device, &rate_address, &wanted_rate)
            .map_err(|_| Self::unsupported(input, "device rejected the nominal sample rate"))?;
        // SAFETY: documented Float64 property.
        let actual_rate: f64 = get_value(self.device, &rate_address)
            .map_err(|_| Self::unsupported(input, "could not read back the nominal sample rate"))?;
        if actual_rate != wanted_rate {
            return Err(Self::unsupported(
                input,
                "device did not settle on the exact requested sample rate",
            ));
        }

        // (c) Output stream and its exact physical format.
        // SAFETY: output-scope stream list on a device.
        let streams: Vec<AudioObjectID> = get_data(
            self.device,
            &prop_addr(
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map_err(|_| Self::unsupported(input, "could not query the device's output streams"))?;
        let Some(&stream) = streams.first() else {
            return Err(Self::unsupported(input, "device has no output streams"));
        };

        let mut log = debug_log();
        if let Some(log) = &mut log {
            use std::io::Write;
            let _ = writeln!(
                log,
                "configure: {} Hz / {} ch / {:?}",
                input.rate_hz, input.layout.channels, input.encoding
            );
        }

        let offered_address = prop_addr(
            kAudioStreamPropertyAvailablePhysicalFormats,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: documented array-of-AudioStreamRangedDescription property
        // on a stream (NOT plain ASBDs: each entry carries a sample-rate
        // range after the format description).
        let offered: Vec<AudioStreamRangedDescription> = get_data(stream, &offered_address)
            .map_err(|_| Self::unsupported(input, "could not query available physical formats"))?;
        if let Some(log) = &mut log {
            use std::io::Write;
            for d in &offered {
                let a = &d.mFormat;
                let _ = writeln!(
                    log,
                    "  offered: rate={} range=[{}, {}] flags=0x{:x} bpf={} ch={} bits={} id=0x{:x}",
                    a.mSampleRate,
                    d.mSampleRateRange.mMinimum,
                    d.mSampleRateRange.mMaximum,
                    a.mFormatFlags,
                    a.mBytesPerFrame,
                    a.mChannelsPerFrame,
                    a.mBitsPerChannel,
                    a.mFormatID,
                );
            }
        }
        let candidates: Vec<CandidateFormat> = offered
            .iter()
            .map(|d| {
                let a = &d.mFormat;
                // When mSampleRate is kAudioStreamAnyRate (0.0) the supported
                // rates are the inclusive range; otherwise the range
                // degenerates to mSampleRate itself.
                let (rate_min, rate_max) = if a.mSampleRate == 0.0 {
                    (d.mSampleRateRange.mMinimum, d.mSampleRateRange.mMaximum)
                } else {
                    (a.mSampleRate, a.mSampleRate)
                };
                CandidateFormat {
                    rate_min,
                    rate_max,
                    format_id: a.mFormatID,
                    format_flags: a.mFormatFlags,
                    bytes_per_frame: a.mBytesPerFrame,
                    channels_per_frame: a.mChannelsPerFrame,
                    bits_per_channel: a.mBitsPerChannel,
                }
            })
            .collect();
        let Some(matched) = pick_physical_format(&candidates, input) else {
            return Err(Self::unsupported(
                input,
                "no offered physical format preserves the stream exactly",
            ));
        };
        // Pin the exact requested rate on the chosen physical format; the
        // readback below verifies the hardware settled on it.
        let mut chosen = offered[matched.index].mFormat;
        chosen.mSampleRate = wanted_rate;

        let physical_address = prop_addr(
            kAudioStreamPropertyPhysicalFormat,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: documented single-ASBD property on a stream.
        set_value(stream, &physical_address, &chosen)
            .map_err(|_| Self::unsupported(input, "device rejected the exact physical format"))?;
        // SAFETY: documented single-ASBD property on a stream.
        let readback: AudioStreamBasicDescription = get_value(stream, &physical_address)
            .map_err(|_| Self::unsupported(input, "could not read back the physical format"))?;
        // Fidelity contract: every field must come back exactly as requested.
        // (mReserved is required to be 0 by the HAL.)
        if let Some(log) = &mut log {
            use std::io::Write;
            let _ = writeln!(log, "  chosen: {chosen:?}");
            let _ = writeln!(log, "  readback: {readback:?}");
        }
        if readback != chosen {
            return Err(Self::unsupported(
                input,
                "physical format readback does not match the requested format",
            ));
        }

        // (d) Device buffer frame size: best-effort, actual is reported.
        let buffer_address = prop_addr(
            kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: documented UInt32 property.
        match set_value(self.device, &buffer_address, &buffers.buffer_frames) {
            Ok(()) => {
                // SAFETY: documented UInt32 property.
                let actual: u32 = get_value(self.device, &buffer_address).unwrap_or(0);
                if let Some(log) = &mut log {
                    use std::io::Write;
                    let _ = writeln!(
                        log,
                        "  buffer frame size {actual} (requested {})",
                        buffers.buffer_frames
                    );
                }
            }
            Err(_) => {
                if let Some(log) = &mut log {
                    use std::io::Write;
                    let _ = writeln!(
                        log,
                        "  buffer frame size {} rejected; device default in use",
                        buffers.buffer_frames
                    );
                }
            }
        }

        // (e) Changing the buffer frame size can make a driver reset the
        // stream to a default format/rate. Re-verify both and re-apply the
        // physical format once; a second mismatch is a hard error because the
        // device would otherwise render a different rate/layout than the
        // ring carries (audible as wrong speed or garbled samples).
        // SAFETY: documented Float64 property.
        let settled_rate: f64 = get_value(self.device, &rate_address)
            .map_err(|_| Self::unsupported(input, "could not re-read the nominal sample rate"))?;
        // SAFETY: documented single-ASBD property on a stream.
        let settled: AudioStreamBasicDescription = get_value(stream, &physical_address)
            .map_err(|_| Self::unsupported(input, "could not re-read the physical format"))?;
        if let Some(log) = &mut log {
            use std::io::Write;
            let _ = writeln!(log, "  settled rate={settled_rate} format={settled:?}");
        }
        if settled != chosen || settled_rate != wanted_rate {
            // SAFETY: documented Float64 property.
            set_value(self.device, &rate_address, &wanted_rate)
                .map_err(|_| Self::unsupported(input, "device rejected the nominal sample rate"))?;
            // SAFETY: documented single-ASBD property on a stream.
            set_value(stream, &physical_address, &chosen)
                .map_err(|_| Self::unsupported(input, "device rejected the exact physical format"))?;
            // SAFETY: documented single-ASBD property on a stream.
            let final_readback: AudioStreamBasicDescription = get_value(stream, &physical_address)
                .map_err(|_| Self::unsupported(input, "could not read back the physical format"))?;
            if let Some(log) = &mut log {
                use std::io::Write;
                let _ = writeln!(log, "  re-applied; final readback: {final_readback:?}");
            }
            if final_readback != chosen {
                return Err(Self::unsupported(
                    input,
                    "device does not keep the exact physical format",
                ));
            }
        }
        if let Some(log) = &mut log {
            use std::io::Write;
            // A nominal-rate readback is not a measurement of the running
            // hardware clock. This property is optional on some endpoints.
            let address = prop_addr(
                kAudioDevicePropertyActualSampleRate,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            );
            // SAFETY: actual sample rate is a Float64 property on the device.
            let actual_rate: Result<f64, i32> = get_value(self.device, &address);
            let _ = writeln!(log, "  actual rate before start: {actual_rate:?}");
        }

        let spec = OutputSpec {
            device: self.device_id.clone(),
            rate_hz: input.rate_hz,
            layout: input.layout,
            format: matched.format,
            valid_bits: matched.valid_bits,
        };
        self.spec = Some(spec.clone());
        Ok(spec)
    }
}

impl AudioOutput for CoreAudioOutput {
    fn configure(
        &mut self,
        input: &StreamSpec,
        buffers: BufferConfig,
    ) -> Result<OutputSpec, PlayerError> {
        // Hog mode first: exclusive access, or nothing (never fall back to
        // shared mixing). Any failure below releases the hog before
        // returning.
        self.acquire_hog()?;
        match self.configure_inner(input, buffers) {
            Ok(spec) => Ok(spec),
            Err(err) => {
                self.release_hog();
                Err(err)
            }
        }
    }

    fn start(
        &mut self,
        pcm: rtrb::Consumer<u8>,
        counters: Arc<OutputCounters>,
    ) -> Result<(), PlayerError> {
        if self.io_proc.is_some() {
            // Already streaming; start is idempotent.
            return Ok(());
        }
        let spec = self
            .spec
            .as_ref()
            .ok_or(PlayerError::InvalidInput(
                "configure must succeed before start",
            ))?;
        let frame_bytes = spec.bytes_per_frame() as u32;
        let rate_hz = spec.rate_hz;

        // Capture everything the callback needs before starting the device.
        // Only the one-time thread-priority request is performed in the
        // callback in addition to bulk ring reads and HAL buffer writes.
        let promoted = Cell::new(false);
        let rt_handle: Cell<Option<audio_thread_priority::RtPriorityHandle>> = Cell::new(None);
        let consumer = RtConsumer(std::cell::UnsafeCell::new(pcm));
        let log_enabled = std::env::var_os("SOINTTY_COREAUDIO_LOG").is_some();
        self.counters = log_enabled.then(|| Arc::clone(&counters));
        let report = Arc::new(RenderReport::new(frame_bytes));
        self.report = Some(Arc::clone(&report));
        let io_block = move |_: NonNull<AudioTimeStamp>,
                             _: NonNull<AudioBufferList>,
                             _: NonNull<AudioTimeStamp>,
                             out: NonNull<AudioBufferList>,
                             _: NonNull<AudioTimeStamp>| {
            io_proc(
                &consumer,
                &counters,
                &report,
                frame_bytes,
                rate_hz,
                &promoted,
                &rt_handle,
                log_enabled,
                out,
            );
        };
        let block: RcBlock<IoBlockDyn> = RcBlock::new(io_block);

        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `proc_id` is a valid out-pointer; the block pointer comes
        // from a live `RcBlock` we keep alive in `self` until after
        // `AudioDeviceDestroyIOProcID`. Passing no dispatch queue means the
        // HAL invokes the block directly on its render thread, which is what
        // the RT discipline assumes. The HAL Block_copy's the block.
        let status = unsafe {
            AudioDeviceCreateIOProcIDWithBlock(
                NonNull::new(&mut proc_id).expect("non-null out-proc reference"),
                self.device,
                None,
                RcBlock::as_ptr(&block),
            )
        };
        if status != kAudioHardwareNoError {
            return Err(PlayerError::Output);
        }
        // SAFETY: `proc_id` was just created for `self.device`.
        let status = unsafe { AudioDeviceStart(self.device, proc_id) };
        if status != kAudioHardwareNoError {
            // SAFETY: `proc_id` is a live IOProcID for `self.device`.
            unsafe { AudioDeviceDestroyIOProcID(self.device, proc_id) };
            return Err(PlayerError::Output);
        }
        self.io_proc = proc_id;
        self.block = Some(block);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        let mut first_error = None;
        // `AudioDeviceIOProcID` is itself an `Option<fn pointer>`; the whole
        // value is the opaque ID (`AudioDeviceIOProcID: Copy`).
        let proc_id = self.io_proc.take();
        if proc_id.is_some() {
            // SAFETY: `proc_id` is a started (or at least created) IOProcID
            // for `self.device`.
            let status = unsafe { AudioDeviceStop(self.device, proc_id) };
            if status != kAudioHardwareNoError {
                first_error = Some(PlayerError::Output);
            }
            // SAFETY: balancing destroy for the created IOProcID; after this
            // the HAL has released its block copy, so dropping our `RcBlock`
            // below is safe.
            let status = unsafe { AudioDeviceDestroyIOProcID(self.device, proc_id) };
            if status != kAudioHardwareNoError && first_error.is_none() {
                first_error = Some(PlayerError::Output);
            }
        }
        // Drop our block reference only after the HAL released its copy.
        self.block = None;
        self.release_hog();
        // The HAL callback has stopped; filesystem I/O and error reporting
        // are safe here, never on its render thread.
        let report = self.report.take();
        let mut promotion_logged = false;
        if let (Some(mut log), Some(counters), Some(report)) =
            (debug_log(), self.counters.take(), report.as_ref())
        {
            use std::io::Write;
            let buffer_count = report.buffer_count.load(Ordering::Acquire);
            if buffer_count > 0 {
                let _ = writeln!(
                    log,
                    "  io: buffers={buffer_count} frame_bytes={}",
                    report.frame_bytes
                );
                for i in 0..(buffer_count as usize).min(LOGGED_BUFFERS) {
                    let bytes = report.buffer_bytes[i].load(Ordering::Relaxed);
                    let _ = writeln!(log, "  io: buffer[{i}] bytes={bytes}");
                }
                if buffer_count as usize > LOGGED_BUFFERS {
                    let _ = writeln!(log, "  io: remaining buffer sizes omitted");
                }
            }
            if report.promotion_failed.load(Ordering::Relaxed) {
                promotion_logged = writeln!(log, "  io: render-thread promotion failed").is_ok();
            }
            let first_xrun_frame = match report.first_xrun_frame.load(Ordering::Relaxed) {
                u64::MAX => None,
                frame => Some(frame),
            };
            let _ = writeln!(
                log,
                "  stop: played_frames={} xruns={} first_xrun_frame={first_xrun_frame:?}",
                counters.played_frames.load(Ordering::Relaxed),
                counters.xruns.load(Ordering::Relaxed),
            );
        }
        if report
            .as_ref()
            .is_some_and(|report| report.promotion_failed.load(Ordering::Relaxed))
            && !promotion_logged
        {
            eprintln!("sointty-output-coreaudio: failed to promote render thread to real-time");
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// The HAL invokes a device's IO blocks serially — with a NULL dispatch
/// queue the block is directly invoked on the HAL's single render thread —
/// so `rtrb`'s `&mut self` consumer is wrapped in interior mutability without
/// a lock. This is the only interior mutability on the RT path.
struct RtConsumer(std::cell::UnsafeCell<rtrb::Consumer<u8>>);

impl RtConsumer {
    fn pop_partial_slice_uninit<'a>(
        &self,
        dst: &'a mut [MaybeUninit<u8>],
    ) -> (&'a mut [u8], &'a mut [MaybeUninit<u8>]) {
        // SAFETY: the HAL invokes this block serially; the block holds the
        // only consumer handle. `dst` is the current HAL output buffer.
        unsafe { &mut *self.0.get() }.pop_partial_slice_uninit(dst)
    }
}

/// Real-time render callback. Runs on the HAL's render thread.
///
/// No filesystem I/O, formatting or locks: copies ring bytes directly into
/// HAL buffers and stores diagnostic atomics. Thread priority is requested
/// once, before sustained rendering; failures are reported by `stop`.
#[allow(clippy::too_many_arguments)]
fn io_proc(
    consumer: &RtConsumer,
    counters: &OutputCounters,
    report: &RenderReport,
    frame_bytes: u32,
    rate_hz: u32,
    promoted: &Cell<bool>,
    rt_handle: &Cell<Option<audio_thread_priority::RtPriorityHandle>>,
    log_enabled: bool,
    out: NonNull<AudioBufferList>,
) {
    // SAFETY: the HAL guarantees `out` points at a valid AudioBufferList for
    // the current IO cycle, with `mNumberBuffers` buffers whose `mData` is
    // either null (stream disabled) or writable for `mDataByteSize` bytes.
    // The flexible `mBuffers` array is the documented C pattern.
    let list = unsafe { out.as_ref() };

    if !promoted.replace(true) {
        // Best-effort time-constraint promotion. Report failures after the
        // render block is destroyed, not via stderr on the HAL thread.
        match audio_thread_priority::promote_current_thread_to_real_time(0, rate_hz) {
            Ok(handle) => rt_handle.set(Some(handle)),
            Err(_) => report.promotion_failed.store(true, Ordering::Relaxed),
        }
        if log_enabled {
            for i in 0..(list.mNumberBuffers as usize).min(LOGGED_BUFFERS) {
                // SAFETY: `i < mNumberBuffers`, inside the HAL's flexible
                // buffer array. Only the byte count is retained.
                let buffer = unsafe { list.mBuffers.as_ptr().add(i).read() };
                report.buffer_bytes[i].store(buffer.mDataByteSize, Ordering::Relaxed);
            }
            report
                .buffer_count
                .store(list.mNumberBuffers, Ordering::Release);
        }
    }

    if frame_bytes == 0 {
        return;
    }
    let frame_bytes = frame_bytes as usize;

    for i in 0..list.mNumberBuffers as usize {
        // SAFETY: `i < mNumberBuffers`, inside the flexible array the HAL
        // allocated.
        let buffer = unsafe { list.mBuffers.as_ptr().add(i).read() };
        let capacity = buffer.mDataByteSize as usize;
        if buffer.mData.is_null() || capacity == 0 {
            continue;
        }
        let frames = capacity / frame_bytes;
        let needed = frames * frame_bytes;
        // SAFETY: `needed <= mDataByteSize`, and this callback alone owns the
        // writable HAL output buffer. Its old contents may be uninitialized;
        // `pop_partial_slice_uninit` initializes only the copied prefix.
        let dst = unsafe {
            std::slice::from_raw_parts_mut(buffer.mData.cast::<MaybeUninit<u8>>(), needed)
        };
        let (filled, shortage) = consumer.pop_partial_slice_uninit(dst);
        let complete_frames = filled.len() / frame_bytes;
        if !shortage.is_empty() {
            // The HAL requires the rest of its output buffer to be silent.
            // SAFETY: this is the writable suffix not filled from the ring.
            unsafe { std::ptr::write_bytes(shortage.as_mut_ptr(), 0, shortage.len()) };
            if log_enabled {
                let frame = counters.played_frames.load(Ordering::Relaxed) + complete_frames as u64;
                let _ = report.first_xrun_frame.compare_exchange(
                    u64::MAX,
                    frame,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
            counters.xruns.fetch_add(1, Ordering::Relaxed);
        }
        counters
            .played_frames
            .fetch_add(complete_frames as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_audio_types::AudioBuffer;

    #[test]
    fn callback_bulk_copies_wrapped_frames_and_silences_only_shortage() {
        let (mut producer, consumer) = rtrb::RingBuffer::<u8>::new(48);
        let bytes: Vec<u8> = (0..56u8)
            .map(|n| n.wrapping_mul(17).wrapping_add(3))
            .collect();
        for &byte in &bytes[..40] {
            producer.push(byte).unwrap();
        }

        let consumer = RtConsumer(std::cell::UnsafeCell::new(consumer));
        let counters = OutputCounters::default();
        let report = RenderReport::new(8);
        let promoted = Cell::new(true); // No OS priority request in this test.
        let rt_handle = Cell::new(None);
        let mut output = vec![MaybeUninit::<u8>::uninit(); 40];
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: 40,
                mData: output.as_mut_ptr().cast(),
            }],
        };

        io_proc(
            &consumer,
            &counters,
            &report,
            8,
            96_000,
            &promoted,
            &rt_handle,
            true,
            NonNull::from(&mut list),
        );
        // SAFETY: io_proc wrote all 40 bytes, either from the ring or as zeroes.
        let first: Vec<u8> = output.iter().map(|b| unsafe { b.assume_init() }).collect();
        assert_eq!(first, bytes[..40]);
        assert_eq!(counters.xruns(), 0);

        // The 48-byte ring wraps after the initial 40-byte read. Only two
        // frames remain for the next five-frame HAL buffer.
        for &byte in &bytes[40..] {
            producer.push(byte).unwrap();
        }
        io_proc(
            &consumer,
            &counters,
            &report,
            8,
            96_000,
            &promoted,
            &rt_handle,
            true,
            NonNull::from(&mut list),
        );
        // SAFETY: io_proc wrote the 16-byte prefix and zero-filled the rest.
        let second: Vec<u8> = output.iter().map(|b| unsafe { b.assume_init() }).collect();
        assert_eq!(&second[..16], &bytes[40..]);
        assert_eq!(&second[16..], &[0; 24]);
        assert_eq!(counters.played_frames(), 7);
        assert_eq!(counters.xruns(), 1);
        assert_eq!(report.first_xrun_frame.load(Ordering::Relaxed), 7);
    }
}
