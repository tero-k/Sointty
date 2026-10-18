//! Shared-mode macOS output through Apple's HAL output audio unit
//! (`kAudioUnitSubType_HALOutput`), the explicit `shared_playback` opt-in.
//!
//! Unlike the direct backend (`hal.rs`), this path never acquires hog mode,
//! never writes device-global nominal/physical/virtual formats, and never
//! installs a raw device IOProc: the unit pulls interleaved F32 client PCM
//! through a render callback and Core Audio mixes it with other clients on
//! the same endpoint. The OS may convert, resample, remix and apply volume
//! processing; shared playback is never bit-perfect and is never presented
//! as such.
//!
//! The render callback obeys the same real-time discipline as the direct
//! backend's IOProc: no allocation, locks, formatting or I/O, only bounded
//! bulk copies from the lock-free ring into the unit's buffers and silence
//! padding on shortage.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use objc2_audio_toolbox::{
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit, AudioUnitGetProperty, AudioUnitInitialize,
    AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
};
use objc2_core_audio::{
    AudioObjectID, kAudioDevicePropertyHogMode, kAudioDevicePropertyStreams, kAudioHardwareNoError,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioStreamPropertyPhysicalFormat,
    kAudioStreamPropertyVirtualFormat,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use sointty_core::{
    AudioOutput, BufferConfig, DeviceFormat, DeviceId, OutputCounters, OutputSpec, PlayerError,
    SampleEncoding, StreamSpec,
};

use crate::format::{FLAG_IS_NON_MIXABLE, K_AUDIO_FORMAT_LINEAR_PCM};
use crate::hal::{get_data, get_value, prop_addr, resolve_device};

/// `kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked`: canonical
/// interleaved F32 client format for the AUHAL input scope.
const CLIENT_FORMAT_FLAGS: u32 = (1 << 0) | (1 << 3);

/// Everything the AUHAL render callback needs. Owned by
/// [`SharedCoreAudioOutput`] behind a stable `Box` whose address is the
/// callback's `inRefCon`; released only after the unit is stopped,
/// uninitialized and disposed (see [`SharedCoreAudioOutput::stop`]).
///
/// The callback is installed during `configure`, before
/// `AudioUnitInitialize`. `consumer` and `counters` are installed in `start`
/// before `AudioOutputUnitStart`; the unit invokes the callback only after
/// that call, so the writes happen-before every read. The unit's render
/// thread is the callback's only caller, so the `&mut` consumer needs no
/// lock (same pattern as the direct backend's `RtConsumer`).
struct SharedRenderState {
    consumer: UnsafeCell<Option<rtrb::Consumer<u8>>>,
    counters: Option<Arc<OutputCounters>>,
    frame_bytes: u32,
}

/// Shared (OS-mixed) Core Audio output on a selected endpoint.
///
/// `Send` is implemented manually: the `AudioUnit` handle is a raw pointer,
/// but every call into it happens on the thread owning `self` (configure,
/// start, stop); only the render callback runs on the unit's own thread and
/// it only touches [`SharedRenderState`], whose contents are all safe to
/// use there.
pub struct SharedCoreAudioOutput {
    device: AudioObjectID,
    device_id: DeviceId,
    unit: Option<AudioUnit>,
    spec: Option<OutputSpec>,
    /// Callback state, allocated during `configure` before unit initialize.
    state: Option<Box<SharedRenderState>>,
    started: bool,
}

// SAFETY: see the struct documentation.
unsafe impl Send for SharedCoreAudioOutput {}

fn unsupported(input: &StreamSpec, reason: &'static str) -> PlayerError {
    PlayerError::UnsupportedFormat {
        rate_hz: input.rate_hz,
        channels: input.layout.channels,
        encoding: input.encoding,
        reason,
    }
}

/// The interleaved F32 client ASBD the unit is asked to pull at the decoded
/// stream's exact rate and channel count.
fn client_asbd(input: &StreamSpec) -> AudioStreamBasicDescription {
    let frame_bytes = u32::from(input.layout.channels) * 4;
    AudioStreamBasicDescription {
        mSampleRate: f64::from(input.rate_hz),
        mFormatID: K_AUDIO_FORMAT_LINEAR_PCM,
        mFormatFlags: CLIENT_FORMAT_FLAGS,
        mBytesPerPacket: frame_bytes,
        mFramesPerPacket: 1,
        mBytesPerFrame: frame_bytes,
        mChannelsPerFrame: u32::from(input.layout.channels),
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Read one property off the audio unit.
///
/// # Safety
///
/// `T` must be exactly the type the unit documents for `id`/`scope`/
/// `element`, and `unit` must be a live instance.
unsafe fn unit_get<T: Copy>(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
) -> Result<T, PlayerError> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: upheld by the caller contract; out pointers are valid stack
    // references for the duration of the call.
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            id,
            scope,
            element,
            NonNull::new(value.as_mut_ptr().cast::<c_void>()).expect("non-null out-data"),
            NonNull::new(&mut size).expect("non-null io-size"),
        )
    };
    if status != kAudioHardwareNoError {
        return Err(PlayerError::Output);
    }
    // SAFETY: the unit reported success and wrote `size_of::<T>()` bytes.
    Ok(unsafe { value.assume_init() })
}

/// Write one property on the audio unit.
///
/// # Safety
///
/// `T` must be exactly the type the unit documents for `id`/`scope`/
/// `element`, and `unit` must be a live instance.
unsafe fn unit_set<T: Copy>(
    unit: AudioUnit,
    id: u32,
    scope: u32,
    element: u32,
    value: &T,
) -> Result<(), PlayerError> {
    // SAFETY: upheld by the caller contract; `value` points to one
    // initialized `T` the unit reads during the call.
    let status = unsafe {
        AudioUnitSetProperty(
            unit,
            id,
            scope,
            element,
            NonNull::from(value).cast::<c_void>().as_ptr(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if status != kAudioHardwareNoError {
        return Err(PlayerError::Output);
    }
    Ok(())
}

impl SharedCoreAudioOutput {
    /// `device`: `"default"` for the default render endpoint, or the decimal
    /// `AudioObjectID` (same selection as the direct backend).
    pub fn new(device: &DeviceId) -> Result<Self, PlayerError> {
        let (object, device_id) = resolve_device(device)?;
        Ok(Self {
            device: object,
            device_id,
            unit: None,
            spec: None,
            state: None,
            started: false,
        })
    }

    /// The endpoint must be shareable right now: not hogged by another
    /// process, and settled into mixable physical/virtual formats. A device
    /// still in a non-mixable integer format (e.g. moments after a direct
    /// hog-mode stream released it) is reported as not settled; the caller's
    /// bounded retry covers that window.
    fn check_mixable(&self, input: &StreamSpec) -> Result<(), PlayerError> {
        let hog_address = prop_addr(
            kAudioDevicePropertyHogMode,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        );
        // SAFETY: documented pid_t property on audio devices.
        let owner: i32 = get_value(self.device, &hog_address).map_err(|_| PlayerError::Output)?;
        if owner != -1 && owner != std::process::id() as i32 {
            return Err(PlayerError::DeviceBusy);
        }

        // SAFETY: documented output-scope stream list on a device.
        let streams: Vec<AudioObjectID> = get_data(
            self.device,
            &prop_addr(
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map_err(|_| PlayerError::Output)?;
        let Some(&stream) = streams.first() else {
            return Err(unsupported(input, "device has no output streams"));
        };
        // SAFETY: documented single-ASBD properties on an output stream.
        let physical: AudioStreamBasicDescription = get_value(
            stream,
            &prop_addr(
                kAudioStreamPropertyPhysicalFormat,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map_err(|_| PlayerError::Output)?;
        // SAFETY: documented single-ASBD properties on an output stream.
        let virtual_format: AudioStreamBasicDescription = get_value(
            stream,
            &prop_addr(
                kAudioStreamPropertyVirtualFormat,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map_err(|_| PlayerError::Output)?;
        let non_mixable =
            |asbd: &AudioStreamBasicDescription| asbd.mFormatFlags & FLAG_IS_NON_MIXABLE != 0;
        if non_mixable(&physical) || non_mixable(&virtual_format) {
            return Err(unsupported(
                input,
                "device has not settled into a mixable format",
            ));
        }
        Ok(())
    }
}

impl AudioOutput for SharedCoreAudioOutput {
    fn configure(
        &mut self,
        input: &StreamSpec,
        _buffers: BufferConfig,
    ) -> Result<OutputSpec, PlayerError> {
        self.stop()?;
        if input.encoding == SampleEncoding::Dsd {
            return Err(unsupported(input, "shared mixing cannot carry DSD"));
        }
        // Do not declare a shared configuration on an endpoint that is
        // hogged by another process or still in a non-mixable format.
        self.check_mixable(input)?;

        // Open Apple's HAL output unit.
        let description = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        // SAFETY: `description` is a valid stack reference; null component
        // starts the search.
        let component =
            unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&description)) };
        if component.is_null() {
            return Err(PlayerError::Output);
        }
        let mut unit: AudioUnit = std::ptr::null_mut();
        // SAFETY: `component` was just found; `unit` is a valid out-pointer.
        let status = unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) };
        if status != kAudioHardwareNoError || unit.is_null() {
            return Err(PlayerError::Output);
        }
        let mut state = Box::new(SharedRenderState {
            consumer: UnsafeCell::new(None),
            counters: None,
            frame_bytes: client_asbd(input).mBytesPerFrame,
        });
        // Every failure below disposes the fresh instance.
        let configured = (|| {
            // Output IO on bus 0 enabled, input on bus 1 disabled (HALOutput
            // defaults already say so; set them explicitly).
            // SAFETY: EnableIO is a documented UInt32 property.
            unsafe {
                unit_set(
                    unit,
                    kAudioOutputUnitProperty_EnableIO,
                    kAudioUnitScope_Output,
                    0,
                    &1_u32,
                )
            }?;
            // SAFETY: EnableIO is a documented UInt32 property.
            unsafe {
                unit_set(
                    unit,
                    kAudioOutputUnitProperty_EnableIO,
                    kAudioUnitScope_Input,
                    1,
                    &0_u32,
                )
            }?;
            // Route the unit to the selected endpoint (a per-instance
            // property; the device's global state is never written).
            // SAFETY: CurrentDevice is a documented AudioObjectID property.
            unsafe {
                unit_set(
                    unit,
                    kAudioOutputUnitProperty_CurrentDevice,
                    kAudioUnitScope_Global,
                    0,
                    &self.device,
                )
            }
            .map_err(|_| PlayerError::DeviceLost)?;
            // Client format: interleaved F32 at the source rate/channels on
            // the unit's input scope, bus 0. The OS converts behind it; no
            // resampling happens in our callback.
            let wanted = client_asbd(input);
            // SAFETY: StreamFormat is a documented ASBD property.
            unsafe {
                unit_set(
                    unit,
                    kAudioUnitProperty_StreamFormat,
                    kAudioUnitScope_Input,
                    0,
                    &wanted,
                )
            }
            .map_err(|_| {
                unsupported(
                    input,
                    "AUHAL rejected the client rate/layout for shared output",
                )
            })?;
            // SAFETY: readback of the same property.
            let readback: AudioStreamBasicDescription = unsafe {
                unit_get(
                    unit,
                    kAudioUnitProperty_StreamFormat,
                    kAudioUnitScope_Input,
                    0,
                )
            }
            .map_err(|_| unsupported(input, "could not read back the shared client format"))?;
            if readback != wanted {
                return Err(unsupported(
                    input,
                    "AUHAL did not accept the exact shared client F32 format",
                ));
            }
            let callback = AURenderCallbackStruct {
                inputProc: Some(render_callback),
                inputProcRefCon: state.as_mut() as *mut SharedRenderState as *mut c_void,
            };
            // SAFETY: `state` has a stable heap address, retained until
            // after the unit is stopped, uninitialized and disposed.
            unsafe {
                unit_set(
                    unit,
                    kAudioUnitProperty_SetRenderCallback,
                    kAudioUnitScope_Input,
                    0,
                    &callback,
                )
            }?;
            // SAFETY: the unit now has its client ASBD and render callback.
            let status = unsafe { AudioUnitInitialize(unit) };
            if status != kAudioHardwareNoError {
                return Err(unsupported(
                    input,
                    "AUHAL could not initialize the shared client rate/layout",
                ));
            }
            Ok(())
        })();
        if let Err(error) = configured {
            // SAFETY: `unit` is a live instance we just created.
            unsafe {
                let _ = AudioUnitUninitialize(unit);
                let _ = AudioComponentInstanceDispose(unit);
            }
            return Err(error);
        }

        // Client-side F32 claim only: what the DAC does after the OS mixer
        // is deliberately not represented here.
        let spec = OutputSpec {
            device: self.device_id.clone(),
            rate_hz: input.rate_hz,
            layout: input.layout,
            format: DeviceFormat::F32Le,
            valid_bits: 32,
        };
        self.unit = Some(unit);
        self.state = Some(state);
        self.spec = Some(spec.clone());
        Ok(spec)
    }

    fn start(
        &mut self,
        consumer: rtrb::Consumer<u8>,
        counters: Arc<OutputCounters>,
    ) -> Result<(), PlayerError> {
        if self.started {
            return Ok(());
        }
        let unit = self.unit.ok_or(PlayerError::Output)?;
        let state = self.state.as_mut().ok_or(PlayerError::Output)?;
        // SAFETY: the unit is initialized but not started, so no render
        // callback can be accessing `state` yet. Both fields are installed
        // before `AudioOutputUnitStart` makes the callback runnable.
        unsafe { *state.consumer.get() = Some(consumer) };
        state.counters = Some(counters);
        // SAFETY: the unit is initialized and already has the callback.
        let status = unsafe { AudioOutputUnitStart(unit) };
        if status != kAudioHardwareNoError {
            return Err(PlayerError::Output);
        }
        self.started = true;
        Ok(())
    }

    /// Stop, uninitialize and dispose the unit before releasing the callback
    /// state, so the render callback can never run against freed memory.
    fn stop(&mut self) -> Result<(), PlayerError> {
        if let Some(unit) = self.unit.take() {
            if self.started {
                // SAFETY: the unit was started by `start`.
                unsafe {
                    let _ = AudioOutputUnitStop(unit);
                }
                self.started = false;
            }
            // Detach the callback before the unit (and then the state) goes
            // away.
            let cleared = AURenderCallbackStruct {
                inputProc: None,
                inputProcRefCon: std::ptr::null_mut(),
            };
            // SAFETY: documented removal of the render callback.
            unsafe {
                let _ = AudioUnitSetProperty(
                    unit,
                    kAudioUnitProperty_SetRenderCallback,
                    kAudioUnitScope_Input,
                    0,
                    NonNull::from(&cleared).cast::<c_void>().as_ptr(),
                    std::mem::size_of::<AURenderCallbackStruct>() as u32,
                );
                let _ = AudioUnitUninitialize(unit);
                let _ = AudioComponentInstanceDispose(unit);
            }
        }
        // The unit can no longer invoke the callback; release its state.
        self.state = None;
        self.spec = None;
        Ok(())
    }
}

impl Drop for SharedCoreAudioOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// AUHAL render callback, invoked on the unit's real-time render thread.
///
/// No allocation, locks, formatting or I/O: bounded bulk copies from the
/// ring into the unit's buffers, zero-fill on shortage (an xrun), atomic
/// counter updates only.
///
/// # Safety
///
/// Called by Core Audio with a live `inRefCon` (registered in `configure`,
/// deregistered before the state is dropped) and a valid `io_data` buffer
/// list for the current render cycle.
unsafe extern "C-unwind" fn render_callback(
    refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _timestamp: NonNull<AudioTimeStamp>,
    _bus: u32,
    _frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: `refcon` is the `SharedRenderState` box registered in
    // `configure`; `stop` clears the callback and disposes the unit before
    // freeing it.
    let state = unsafe { &*refcon.cast::<SharedRenderState>().as_ptr() };
    let frame_bytes = state.frame_bytes as usize;
    if data.is_null() || frame_bytes == 0 {
        return 0;
    }
    // SAFETY: Core Audio guarantees `data` is a valid AudioBufferList for
    // this render cycle; the flexible `mBuffers` array is the documented C
    // pattern.
    let list = unsafe { &*data };
    for i in 0..list.mNumberBuffers as usize {
        // SAFETY: `i < mNumberBuffers`, inside the unit's buffer array.
        let buffer = unsafe { list.mBuffers.as_ptr().add(i).read() };
        let capacity = buffer.mDataByteSize as usize;
        if buffer.mData.is_null() || capacity == 0 {
            continue;
        }
        let frames = capacity / frame_bytes;
        let needed = frames * frame_bytes;
        // SAFETY: `needed <= mDataByteSize`; this callback alone writes the
        // unit's output buffer.
        let dst = unsafe {
            std::slice::from_raw_parts_mut(buffer.mData.cast::<MaybeUninit<u8>>(), needed)
        };
        // SAFETY: the unit invokes this callback serially on its render
        // thread; the state holds the only consumer handle.
        let consumer = unsafe { &mut *state.consumer.get() };
        let Some(consumer) = consumer else {
            // No consumer installed yet: render silence.
            // SAFETY: `dst` is the unit's writable output buffer.
            unsafe { std::ptr::write_bytes(dst.as_mut_ptr(), 0, needed) };
            if let Some(counters) = &state.counters {
                counters.xruns.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        };
        let (filled, shortage) = consumer.pop_partial_slice_uninit(dst);
        let complete_frames = filled.len() / frame_bytes;
        if !shortage.is_empty() {
            // The unit requires the unfilled suffix to be silent.
            // SAFETY: `shortage` is the writable suffix not filled above.
            unsafe { std::ptr::write_bytes(shortage.as_mut_ptr(), 0, shortage.len()) };
            if let Some(counters) = &state.counters {
                counters.xruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(counters) = &state.counters {
            counters
                .played_frames
                .fetch_add(complete_frames as u64, Ordering::Relaxed);
        }
    }
    0
}
