//! WASAPI exclusive-mode output backend (Windows only).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use sointty_core::{
    AudioOutput, BufferConfig, DeviceId, OutputCounters, OutputSpec, PlayerError, StreamSpec,
    validate_exact_spec,
};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::{
    HANDLE, PROPERTYKEY, RPC_E_CHANGED_MODE, S_FALSE, S_OK, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    DEVICE_STATE_ACTIVE, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToBSTR,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{
    CancelWaitableTimer, CreateEventW, CreateWaitableTimerW, SetEvent, SetWaitableTimer,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::core::{Error, GUID, HSTRING};

use crate::candidates::{
    CandidateDesc, SUBTYPE_IEEE_FLOAT, SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE,
    WAVEFORMATEXTENSIBLE_EXTRA, WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM, block_align,
    candidates_for, channel_mask, frames_to_hns,
};

/// `PKEY_Device_FriendlyName` (defined locally to avoid pulling in the
/// `Win32_Devices_FunctionDiscovery` feature).
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// `AUDCLNT_E_INVALID_DEVICE_PERIOD` / `AUDCLNT_E_BUFFER_SIZE_ERROR` per the
/// Windows SDK (`audioclient.h`, `AUDCLNT_ERR(0x013)` / `AUDCLNT_ERR(0x019)`).
/// The `windows` crate's generated constants (`0x88890020` / `0x88890016`)
/// do not match the SDK headers, so define them here.
const AUDCLNT_E_INVALID_DEVICE_PERIOD: windows::core::HRESULT =
    windows::core::HRESULT(0x88890013_u32 as _);
const AUDCLNT_E_BUFFER_SIZE_ERROR: windows::core::HRESULT =
    windows::core::HRESULT(0x88890019_u32 as _);

/// Event wait timeout in ms; a device that stays silent this long is faulty.
const EVENT_TIMEOUT_MS: u32 = 2000;

/// COM interface pointers are thread-agnostic when every user thread joins
/// the MTA (COM provides the synchronization); wrap them so `WasapiOutput`
/// can satisfy `AudioOutput: Send`.
struct SendCom<T>(T);

unsafe impl<T> Send for SendCom<T> {}

impl<T: Clone> Clone for SendCom<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> std::ops::Deref for SendCom<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct WasapiOutput {
    device: DeviceId,
    client: Option<SendCom<IAudioClient>>,
    render: Option<SendCom<IAudioRenderClient>>,
    event: Option<SendCom<HANDLE>>,
    spec: Option<OutputSpec>,
    /// Actual device buffer size in frames, queried after `Initialize`.
    buffer_frames: u32,
    /// Negotiated periodicity in frames.
    period_frames: u32,
    /// Render cadence decided in `configure` from the ACTUAL device buffer:
    /// push mode or a driver-forced whole-buffer period both need timer
    /// top-ups. Recomputing this in the render thread from requested values
    /// is wrong when the driver rounds the buffer up (observed: 8820
    /// requested frames became 15360 at 192 kHz, silently disabling the
    /// timer for a push-mode client whose event is never armed).
    timer_mode: bool,
    thread: Option<JoinHandle<()>>,
    /// Set by `stop()`; the render thread checks it after every event wake so
    /// a stop request is never lost to a WASAPI signal or a full-buffer
    /// iteration (a one-shot event pulse could be consumed either way).
    stop: Arc<AtomicBool>,
}

impl WasapiOutput {
    pub fn new(device: &DeviceId) -> Result<Self, PlayerError> {
        if device.is_empty() {
            return Err(PlayerError::InvalidInput("device id must not be empty"));
        }
        if device != "default" {
            // Resolve (and validate) the endpoint now; activation still
            // happens per configure.
            let enumerator = create_enumerator()?;
            unsafe { enumerator.GetDevice(&HSTRING::from(device.as_str())) }
                .map_err(|_| PlayerError::InvalidInput("endpoint not found"))?;
        }
        Ok(Self {
            device: device.clone(),
            client: None,
            render: None,
            event: None,
            spec: None,
            buffer_frames: 0,
            period_frames: 0,
            timer_mode: false,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// `(endpoint id, friendly name)` for all active render endpoints.
    pub fn list_devices() -> Result<Vec<(DeviceId, String)>, PlayerError> {
        let enumerator = create_enumerator()?;
        let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
            .map_err(|_| PlayerError::Output)?;
        let count = unsafe { collection.GetCount() }.map_err(|_| PlayerError::Output)?;
        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let endpoint = unsafe { collection.Item(index) }.map_err(|_| PlayerError::Output)?;
            let id = endpoint_id(&endpoint)?;
            let name = friendly_name(&endpoint).unwrap_or_else(|_| id.clone());
            devices.push((id, name));
        }
        Ok(devices)
    }
}

impl AudioOutput for WasapiOutput {
    fn configure(
        &mut self,
        input: &StreamSpec,
        buffers: BufferConfig,
    ) -> Result<OutputSpec, PlayerError> {
        validate_exact_spec(input)?;
        self.stop()?;

        let enumerator = create_enumerator()?;
        let endpoint = self.resolve_endpoint(&enumerator)?;
        let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None) }
            .map_err(|error| map_activate_error(&error))?;
        let mut client = SendCom(client);

        // Try every candidate, in fidelity order, accepting ONLY an exact
        // S_OK from IsFormatSupported (exclusive mode).
        let mut accepted: Option<(CandidateDesc, AcceptedFormat)> = None;
        'candidates: for candidate in candidates_for(input.encoding) {
            let extensible = build_extensible(input, candidate);
            if is_supported_exact(&client, (&raw const extensible).cast()) {
                accepted = Some((*candidate, AcceptedFormat::Extensible(extensible)));
                break 'candidates;
            }
            // Driver quirk (MS docs): some drivers accept 1/2-channel PCM as a
            // stand-alone WAVEFORMATEX but reject the same format as
            // WAVEFORMATEXTENSIBLE.
            if input.layout.channels <= 2 {
                let plain = build_plain(input, candidate);
                if is_supported_exact(&client, &raw const plain) {
                    accepted = Some((*candidate, AcceptedFormat::Plain(plain)));
                    break 'candidates;
                }
            }
        }
        let (candidate, format) = accepted.ok_or(PlayerError::UnsupportedFormat {
            rate_hz: input.rate_hz,
            channels: input.layout.channels,
            encoding: input.encoding,
            reason: "no exact exclusive format",
        })?;

        let mut min_period = 0_i64;
        unsafe { client.GetDevicePeriod(None, Some(&raw mut min_period)) }
            .map_err(|_| PlayerError::Output)?;
        // Endpoints may force legal timing; clamp requested buffer and
        // periodicity against the device minimum period.
        let buffer_hns = frames_to_hns(buffers.buffer_frames, input.rate_hz).max(min_period);
        let period_hns = frames_to_hns(buffers.period_frames, input.rate_hz)
            .max(min_period)
            .min(buffer_hns);

        let frame_bytes =
            u32::from(input.layout.channels) * u32::from(candidate.container_bits / 8);
        let aligned_buffer_hns = align_buffer_hns(buffer_hns, input.rate_hz, frame_bytes);
        // Endpoints may force legal timing. Negotiate in steps, keeping the
        // stream format exact throughout:
        // 1. requested buffer duration with requested periodicity;
        // 2. periodicity rounded up to whole milliseconds (USB drivers like
        //    the iFi reject fractional-millisecond periods with
        //    AUDCLNT_E_INVALID_DEVICE_PERIOD);
        // 3. periodicity == buffer duration (endpoints that only accept one
        //    event per buffer in exclusive event mode);
        // 4. buffer byte size aligned up to a 128-byte multiple (the Intel
        //    HD Audio requirement, AUDCLNT_E_BUFFER_SIZE_ERROR).
        let attempts = init_attempts(buffer_hns, period_hns, aligned_buffer_hns);
        let mut initialized = None;
        for (attempt_buffer, attempt_period) in attempts {
            let result = unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    attempt_buffer,
                    attempt_period,
                    format.as_waveformatex_ptr(),
                    None,
                )
            };
            match result {
                Ok(()) => {
                    initialized = Some(attempt_period);
                    break;
                }
                Err(error)
                    if error.code() == AUDCLNT_E_INVALID_DEVICE_PERIOD
                        || error.code() == AUDCLNT_E_BUFFER_SIZE_ERROR => {}
                Err(error) => {
                    return Err(if error.code() == AUDCLNT_E_DEVICE_IN_USE {
                        PlayerError::DeviceBusy
                    } else {
                        PlayerError::Output
                    });
                }
            }
        }
        let Some(actual_period_hns) = initialized else {
            return Err(PlayerError::Output);
        };

        // Some USB drivers (observed: iFi) reject any event-mode period below
        // the buffer duration and then report padding as binary full/empty,
        // firing the event only once the buffer is completely exhausted —
        // every write delay becomes an audible click. Retry the stream in
        // push mode (no event callback), where such drivers report granular
        // padding and the render loop tops up on a timer. Fall back to
        // whole-buffer event mode if push mode is rejected.
        let mut push_mode = false;
        if actual_period_hns >= buffer_hns {
            drop(client);
            let push_client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None) }
                .map_err(|error| map_activate_error(&error))?;
            let result = unsafe {
                push_client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    Default::default(),
                    buffer_hns,
                    0,
                    format.as_waveformatex_ptr(),
                    None,
                )
            };
            if result.is_ok() {
                push_mode = true;
                client = SendCom(push_client);
            } else {
                // Push mode rejected; fall back to whole-buffer event mode.
                drop(push_client);
                let event_client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None) }
                    .map_err(|error| map_activate_error(&error))?;
                unsafe {
                    event_client.Initialize(
                        AUDCLNT_SHAREMODE_EXCLUSIVE,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        buffer_hns,
                        buffer_hns,
                        format.as_waveformatex_ptr(),
                        None,
                    )
                }
                .map_err(|_| PlayerError::Output)?;
                client = SendCom(event_client);
            }
        }

        let buffer_frames = unsafe { client.GetBufferSize() }.map_err(|_| PlayerError::Output)?;
        let period_frames =
            (actual_period_hns as u64 * u64::from(input.rate_hz) / 10_000_000) as u32;

        let event =
            unsafe { CreateEventW(None, false, false, None) }.map_err(|_| PlayerError::Output)?;
        if !push_mode {
            unsafe { client.SetEventHandle(event) }.map_err(|_| PlayerError::Output)?;
        }
        let render: IAudioRenderClient =
            unsafe { client.GetService() }.map_err(|_| PlayerError::Output)?;
        // Negotiated parameters are surfaced through the returned `OutputSpec`;
        // printing here would corrupt the TUI's alternate screen.
        let spec = OutputSpec {
            device: self.device.clone(),
            rate_hz: input.rate_hz,
            layout: input.layout,
            format: candidate.format,
            valid_bits: candidate.valid_bits as u8,
        };
        self.client = Some(client);
        self.render = Some(SendCom(render));
        self.event = Some(SendCom(event));
        self.buffer_frames = buffer_frames;
        self.period_frames = period_frames;
        self.timer_mode = push_mode || needs_timer_topup(period_frames, buffer_frames);
        self.spec = Some(spec.clone());
        Ok(spec)
    }

    fn start(
        &mut self,
        mut consumer: rtrb::Consumer<u8>,
        counters: Arc<OutputCounters>,
    ) -> Result<(), PlayerError> {
        if self.thread.is_some() {
            return Err(PlayerError::InvalidInput("output already started"));
        }
        let client = self.client.clone().ok_or(PlayerError::Output)?;
        let render = self.render.clone().ok_or(PlayerError::Output)?;
        let event = self.event.clone().ok_or(PlayerError::Output)?;
        let spec = self.spec.clone().ok_or(PlayerError::Output)?;
        let frame_bytes = spec.bytes_per_frame();
        let rate_hz = spec.rate_hz;
        let buffer_frames = self.buffer_frames.max(1);
        let period_frames = self.period_frames.max(1);
        let timer_mode = self.timer_mode;

        // Preallocate the render scratch before the thread starts; the render
        // loop itself performs no heap allocation.
        let scratch = vec![0_u8; frame_bytes * buffer_frames as usize];
        let stop = self.stop.clone();
        let thread = std::thread::Builder::new()
            .name("sointty-output-wasapi".to_owned())
            .spawn(move || {
                render_loop(
                    client,
                    render,
                    event,
                    rate_hz,
                    buffer_frames,
                    period_frames,
                    timer_mode,
                    scratch,
                    &mut consumer,
                    counters,
                    stop,
                );
            })
            .map_err(|_| PlayerError::Output)?;
        self.thread = Some(thread);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        if let Some(thread) = self.thread.take() {
            // Ask the render loop to exit and wake it if it is waiting on the
            // device event; the flag (not the event pulse) carries the stop
            // request, so it cannot be lost to a WASAPI signal.
            self.stop.store(true, Ordering::Release);
            if let Some(event) = &self.event {
                let _ = unsafe { SetEvent(event.0) };
            }
            let _ = thread.join();
            self.stop.store(false, Ordering::Release);
        }
        if let Some(client) = self.client.take() {
            let _ = unsafe { client.Stop() };
            let _ = unsafe { client.Reset() };
        }
        self.render = None;
        self.event = None;
        self.spec = None;
        self.buffer_frames = 0;
        self.period_frames = 0;
        self.timer_mode = false;
        Ok(())
    }
}

impl Drop for WasapiOutput {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl WasapiOutput {
    fn resolve_endpoint(
        &self,
        enumerator: &IMMDeviceEnumerator,
    ) -> Result<IMMDevice, PlayerError> {
        if self.device == "default" {
            unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .map_err(|_| PlayerError::DeviceLost)
        } else {
            unsafe { enumerator.GetDevice(&HSTRING::from(self.device.as_str())) }
                .map_err(|_| PlayerError::InvalidInput("endpoint not found"))
        }
    }
}

enum AcceptedFormat {
    Extensible(WAVEFORMATEXTENSIBLE),
    Plain(WAVEFORMATEX),
}

impl AcceptedFormat {
    fn as_waveformatex_ptr(&self) -> *const WAVEFORMATEX {
        match self {
            Self::Extensible(format) => (&raw const format.Format).cast(),
            Self::Plain(format) => (&raw const *format).cast(),
        }
    }
}

/// Drivers that reject a period below the buffer duration signal the device
/// event only once per whole buffer, when it is already exhausted. Detect
/// that negotiation so the render loop can top up on a timer instead.
fn needs_timer_topup(period_frames: u32, buffer_frames: u32) -> bool {
    period_frames >= buffer_frames
}

/// Top-up cadence for [`needs_timer_topup`] devices: a quarter buffer per
/// tick keeps at least half the buffer of slack against scheduling jitter.
fn timer_interval_ms(buffer_frames: u32, rate_hz: u32) -> u32 {
    let quarter = u64::from(buffer_frames / 4).max(1);
    (quarter * 1000 / u64::from(rate_hz)).max(1) as u32
}

#[allow(clippy::too_many_arguments)]
fn render_loop(
    client: SendCom<IAudioClient>,
    render: SendCom<IAudioRenderClient>,
    event: SendCom<HANDLE>,
    rate_hz: u32,
    buffer_frames: u32,
    period_frames: u32,
    timer_mode: bool,
    mut scratch: Vec<u8>,
    consumer: &mut rtrb::Consumer<u8>,
    counters: Arc<OutputCounters>,
    stop: Arc<AtomicBool>,
) {
    let event = event.0;
    // These COM interfaces were obtained on another thread; initialize the
    // MTA here (S_FALSE is fine: already initialized).
    let com_hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com_hr.is_err() && com_hr != S_FALSE {
        eprintln!("sointty-output-wasapi: render thread failed to initialize COM");
        counters.fault.store(true, Ordering::Relaxed);
        return;
    }

    // In timer mode the buffer duration doubles as the driver period; give
    // MMCSS the real top-up cadence instead.
    let rt_period_frames = if timer_mode {
        (buffer_frames / 4).max(1)
    } else {
        period_frames
    };
    let rt_handle =
        match audio_thread_priority::promote_current_thread_to_real_time(
            rt_period_frames,
            rate_hz,
        ) {
            Ok(handle) => Some(handle),
            Err(error) => {
                eprintln!(
                    "sointty-output-wasapi: warning: failed to promote render thread \
                     to real-time priority: {error}"
                );
                None
            }
        };

    let frame_bytes = scratch.len() / buffer_frames as usize;

    // Pre-fill the device buffer before starting: drivers that force
    // period == buffer otherwise play one full buffer of driver-owned data
    // before the first event. A short ring supply at startup pads with
    // silence; that is startup latency, not an underrun.
    let (_, shortage) = consumer.pop_partial_slice(&mut scratch[..]);
    shortage.fill(0);
    match unsafe { render.GetBuffer(buffer_frames) } {
        Ok(pointer) => {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    scratch.as_ptr(),
                    pointer,
                    scratch.len(),
                )
            };
            if unsafe { render.ReleaseBuffer(buffer_frames, 0) }.is_err() {
                counters.fault.store(true, Ordering::Relaxed);
                if let Some(handle) = rt_handle {
                    let _ =
                        audio_thread_priority::demote_current_thread_from_real_time(handle);
                }
                return;
            }
        }
        Err(_) => {
            counters.fault.store(true, Ordering::Relaxed);
            if let Some(handle) = rt_handle {
                let _ = audio_thread_priority::demote_current_thread_from_real_time(handle);
            }
            return;
        }
    }

    // Timer-driven top-up covers two driver behaviours: push mode (the event
    // is never armed; the timer is the only cadence) and drivers that force
    // whole-buffer event mode (the event fires only when the buffer is
    // already exhausted, so the timer provides the real cadence and the
    // event is just an extra wake). Quarter-buffer ticks keep at least half
    // the buffer of slack against scheduling jitter.
    let interval_ms = timer_interval_ms(buffer_frames, rate_hz);
    let timer = if timer_mode {
        unsafe { CreateWaitableTimerW(None, false, None) }
            .ok()
            .and_then(|handle| {
                let due = -(i64::from(interval_ms)) * 10_000;
                unsafe {
                    SetWaitableTimer(handle, &due, interval_ms as i32, None, None, false)
                }
                .ok()
                .map(|_| handle)
            })
    } else {
        None
    };

    if let Err(error) = unsafe { client.Start() } {
        eprintln!("sointty-output-wasapi: IAudioClient::Start failed: {error}");
        counters.fault.store(true, Ordering::Relaxed);
        if let Some(handle) = rt_handle {
            let _ = audio_thread_priority::demote_current_thread_from_real_time(handle);
        }
        return;
    }

    let capacity_frames = buffer_frames;

    'stream: loop {
        // No allocation, locks, formatting, or channel traffic in this loop:
        // event wait + device buffer queries + ring pops + device writes only.
        // WAIT_OBJECT_0 = device event, WAIT_OBJECT_0 + 1 = top-up timer. In
        // timer mode without a timer handle, the wait timeout IS the tick.
        let wait = match timer {
            Some(timer) => unsafe {
                WaitForMultipleObjects(&[event, timer], false, EVENT_TIMEOUT_MS)
            },
            None if timer_mode => unsafe { WaitForSingleObject(event, interval_ms) },
            None => unsafe { WaitForSingleObject(event, EVENT_TIMEOUT_MS) },
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        let tick = wait == WAIT_TIMEOUT && timer_mode && timer.is_none();
        if !tick && wait.0 > 1 {
            counters.fault.store(true, Ordering::Relaxed);
            break;
        }
        let padding = match unsafe { client.GetCurrentPadding() } {
            Ok(padding) => padding,
            Err(_) => {
                counters.fault.store(true, Ordering::Relaxed);
                break;
            }
        };
        let available = buffer_frames.saturating_sub(padding).min(capacity_frames);
        if available == 0 {
            continue;
        }
        let bytes = available as usize * frame_bytes;

        let (_, shortage) = consumer.pop_partial_slice(&mut scratch[..bytes]);
        let popped = bytes - shortage.len();
        if !shortage.is_empty() {
            shortage.fill(0);
            if popped == 0 && consumer.is_abandoned() {
                // Producer is gone and the ring is drained; stop quietly.
                break 'stream;
            }
            counters.xruns.fetch_add(1, Ordering::Relaxed);
        }

        let data = &scratch[..bytes];
        let device_buffer = match unsafe { render.GetBuffer(available) } {
            Ok(pointer) => pointer,
            Err(_) => {
                counters.fault.store(true, Ordering::Relaxed);
                break;
            }
        };
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), device_buffer, bytes) };
        if unsafe { render.ReleaseBuffer(available, 0) }.is_err() {
            counters.fault.store(true, Ordering::Relaxed);
            break;
        }
        counters
            .played_frames
            .fetch_add(u64::from(available), Ordering::Relaxed);
    }

    if let Some(timer) = timer {
        unsafe {
            let _ = CancelWaitableTimer(timer);
            let _ = CloseHandle(timer);
        }
    }


    let _ = unsafe { client.Stop() };
    let _ = unsafe { client.Reset() };
    if let Some(handle) = rt_handle {
        let _ = audio_thread_priority::demote_current_thread_from_real_time(handle);
    }
}

fn ensure_mta() -> Result<(), PlayerError> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_ok() || hr == S_FALSE {
        Ok(())
    } else if hr == RPC_E_CHANGED_MODE {
        Err(PlayerError::InvalidInput(
            "thread already initialized with a non-MTA COM apartment",
        ))
    } else {
        Err(PlayerError::Output)
    }
}

fn create_enumerator() -> Result<IMMDeviceEnumerator, PlayerError> {
    ensure_mta()?;
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|_| PlayerError::Output)
}

fn is_supported_exact(client: &IAudioClient, format: *const WAVEFORMATEX) -> bool {
    // Exclusive mode: accept ONLY S_OK. S_FALSE or any failure code means the
    // exact format is not supported; never fall back to a closest match.
    let hr = unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, format, None) };
    hr == S_OK
}

/// `(buffer, period)` initialization attempts in fallback order. A
/// fractional-millisecond period gets a whole-millisecond retry before the
/// whole-buffer last resorts; duplicate steps are skipped.
fn init_attempts(buffer_hns: i64, period_hns: i64, aligned_buffer_hns: i64) -> Vec<(i64, i64)> {
    let mut attempts = vec![(buffer_hns, period_hns)];
    let ms_period = (u64::try_from(period_hns.max(0)).unwrap_or(0).div_ceil(10_000) * 10_000) as i64;
    if ms_period != period_hns && ms_period < buffer_hns {
        attempts.push((buffer_hns, ms_period));
    }
    if period_hns != buffer_hns {
        attempts.push((buffer_hns, buffer_hns));
    }
    if aligned_buffer_hns > buffer_hns {
        attempts.push((aligned_buffer_hns, aligned_buffer_hns));
    }
    attempts
}

/// Round a buffer duration (100-ns units) up so the buffer byte size is a
/// multiple of 128 bytes, the alignment required by Intel HD Audio devices.
fn align_buffer_hns(buffer_hns: i64, rate_hz: u32, frame_bytes: u32) -> i64 {
    let frame_bytes = u64::from(frame_bytes.max(1));
    let frames = (buffer_hns as u64 * u64::from(rate_hz) + 9_999_999) / 10_000_000;
    let buffer_bytes = frames * frame_bytes;
    let aligned_bytes = buffer_bytes.div_ceil(128) * 128;
    let aligned_frames = aligned_bytes / frame_bytes;
    frames_to_hns(aligned_frames as u32, rate_hz)
}

fn build_extensible(input: &StreamSpec, candidate: &CandidateDesc) -> WAVEFORMATEXTENSIBLE {
    let align = block_align(input.layout.channels, candidate);
    let mut format = WAVEFORMATEXTENSIBLE::default();
    format.Format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_EXTENSIBLE,
        nChannels: input.layout.channels,
        nSamplesPerSec: input.rate_hz,
        nAvgBytesPerSec: input.rate_hz * u32::from(align),
        nBlockAlign: align,
        wBitsPerSample: candidate.container_bits,
        cbSize: WAVEFORMATEXTENSIBLE_EXTRA,
    };
    format.Samples.wValidBitsPerSample = candidate.valid_bits;
    format.dwChannelMask = channel_mask(&input.layout);
    format.SubFormat = GUID::from_u128(if candidate.subformat_pcm {
        SUBTYPE_PCM
    } else {
        SUBTYPE_IEEE_FLOAT
    });
    format
}

fn build_plain(input: &StreamSpec, candidate: &CandidateDesc) -> WAVEFORMATEX {
    let align = block_align(input.layout.channels, candidate);
    WAVEFORMATEX {
        wFormatTag: if candidate.subformat_pcm {
            WAVE_FORMAT_PCM
        } else {
            WAVE_FORMAT_IEEE_FLOAT
        },
        nChannels: input.layout.channels,
        nSamplesPerSec: input.rate_hz,
        nAvgBytesPerSec: input.rate_hz * u32::from(align),
        nBlockAlign: align,
        wBitsPerSample: candidate.container_bits,
        cbSize: 0,
    }
}

fn endpoint_id(endpoint: &IMMDevice) -> Result<DeviceId, PlayerError> {
    let id = unsafe { endpoint.GetId() }.map_err(|_| PlayerError::Output)?;
    let name = String::from_utf16_lossy(unsafe { id.as_wide() });
    unsafe { CoTaskMemFree(Some(id.as_ptr().cast())) };
    Ok(name)
}

fn friendly_name(endpoint: &IMMDevice) -> Result<String, PlayerError> {
    let store: IPropertyStore = unsafe { endpoint.OpenPropertyStore(STGM_READ) }
        .map_err(|_| PlayerError::Output)?;
    let mut value: PROPVARIANT = unsafe { store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) }
        .map_err(|_| PlayerError::Output)?;
    let name = unsafe { PropVariantToBSTR(&value) }
        .map_err(|_| PlayerError::Output)
        .map(|name| name.to_string());
    unsafe { PropVariantClear(&raw mut value) }.map_err(|_| PlayerError::Output)?;
    name
}

fn map_activate_error(error: &Error) -> PlayerError {
    if error.code() == AUDCLNT_E_DEVICE_IN_USE {
        PlayerError::DeviceBusy
    } else {
        PlayerError::Output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sointty_core::{ChannelLayout, DeviceFormat, SampleEncoding};

    #[test]
    fn descriptor_layout_matches_extensible_conventions() {
        // Building a descriptor must not touch the exact stream params.
        let input = StreamSpec {
            rate_hz: 96_000,
            layout: ChannelLayout::discrete(2),
            encoding: SampleEncoding::S24,
        };
        let candidate = candidates_for(input.encoding)[1];
        let format = build_extensible(&input, &candidate);
        // `WAVEFORMATEXTENSIBLE` is packed(1): read every field by value.
        let base = format.Format;
        let (tag, channels, rate, bits, cb_size) = (
            base.wFormatTag,
            base.nChannels,
            base.nSamplesPerSec,
            base.wBitsPerSample,
            base.cbSize,
        );
        assert_eq!(tag, WAVE_FORMAT_EXTENSIBLE);
        assert_eq!(channels, 2);
        assert_eq!(rate, 96_000);
        assert_eq!(bits, 32);
        assert_eq!(cb_size, WAVEFORMATEXTENSIBLE_EXTRA);
        let valid_bits = unsafe { format.Samples.wValidBitsPerSample };
        let mask = format.dwChannelMask;
        let sub_format = format.SubFormat;
        assert_eq!(valid_bits, 24);
        assert_eq!(mask, 0x3);
        assert_eq!(sub_format, GUID::from_u128(SUBTYPE_PCM));
    }

    #[test]
    fn plain_descriptor_uses_waveformatex_tags() {
        let input = StreamSpec {
            rate_hz: 44_100,
            layout: ChannelLayout::discrete(2),
            encoding: SampleEncoding::S16,
        };
        let candidate = candidates_for(input.encoding)[0];
        let format = build_plain(&input, &candidate);
        let (tag, channels, rate, bits, cb_size, align, avg) = (
            format.wFormatTag,
            format.nChannels,
            format.nSamplesPerSec,
            format.wBitsPerSample,
            format.cbSize,
            format.nBlockAlign,
            format.nAvgBytesPerSec,
        );
        assert_eq!(tag, WAVE_FORMAT_PCM);
        assert_eq!(channels, 2);
        assert_eq!(rate, 44_100);
        assert_eq!(bits, 16);
        assert_eq!(cb_size, 0);
        assert_eq!(align, 4);
        assert_eq!(avg, 44_100 * 4);

        let input = StreamSpec {
            rate_hz: 48_000,
            layout: ChannelLayout::discrete(1),
            encoding: SampleEncoding::F32,
        };
        let candidate = candidates_for(input.encoding)[0];
        let format = build_plain(&input, &candidate);
        let (tag, channels, align) = (format.wFormatTag, format.nChannels, format.nBlockAlign);
        assert_eq!(tag, WAVE_FORMAT_IEEE_FLOAT);
        assert_eq!(channels, 1);
        assert_eq!(align, 4);
    }

    /// Manual enumeration smoke test: lists active render endpoints and
    /// configures the first one by its explicit endpoint ID.
    #[test]
    #[ignore = "requires real WASAPI endpoints"]
    fn lists_endpoints_and_opens_explicit_device() {
        let devices = WasapiOutput::list_devices().unwrap();
        assert!(!devices.is_empty());
        assert!(
            devices
                .iter()
                .all(|(id, name)| !id.is_empty() && !name.is_empty())
        );
        for (id, name) in &devices {
            eprintln!("id={id} name={name}");
        }
        let (id, _) = devices.into_iter().next().unwrap();
        let mut output = WasapiOutput::new(&id).unwrap();
        let result = output.configure(
            &StreamSpec {
                rate_hz: 44_100,
                layout: ChannelLayout::discrete(2),
                encoding: SampleEncoding::S16,
            },
            BufferConfig::default_for_rate(44_100),
        );
        match result {
            Ok(spec) => {
                assert_eq!(spec.device, id);
                assert_eq!(spec.format, DeviceFormat::S16Le);
                output.stop().unwrap();
            }
            // Individual endpoints may legitimately reject 44.1 kHz stereo
            // S16 exclusive; enumeration itself is what this test proves.
            Err(error) => eprintln!("explicit endpoint configure failed: {error}"),
        }
    }

    #[test]
    fn aligns_buffer_to_128_byte_multiple() {
        // 44.1 kHz stereo S16: 150 ms = 6615 frames = 26460 bytes; the next
        // 128-byte multiple is 26496 bytes = 6624 frames.
        let aligned = align_buffer_hns(1_500_000, 44_100, 4);
        let frames = aligned * 44_100 / 10_000_000;
        assert_eq!(frames * 4 % 128, 0);
        assert!(frames >= 6615);
        assert_eq!(frames, 6624);
        // Re-aligning an aligned duration stays 128-byte aligned and never
        // shrinks the buffer.
        let aligned = align_buffer_hns(frames_to_hns(6624, 44_100), 44_100, 4);
        let frames = aligned * 44_100 / 10_000_000;
        assert_eq!(frames * 4 % 128, 0);
        assert!(frames >= 6624);
    }

    #[test]
    fn timer_topup_only_when_driver_forces_whole_buffer_period() {
        // Normal negotiation: period well below the buffer -> event-driven.
        assert!(!needs_timer_topup(2205, 8820));
        // iFi USB negotiation observed on hardware: the driver clamps the
        // period up to the full buffer duration.
        assert!(needs_timer_topup(8820, 8820));
        assert!(needs_timer_topup(9000, 8820));
    }

    #[test]
    fn timer_interval_is_quarter_buffer_with_floor() {
        // 96 kHz / 8820-frame buffer: 2205 frames = 22 ms.
        assert_eq!(timer_interval_ms(8820, 96_000), 22);
        // 44.1 kHz / 8820-frame buffer: 2205 frames = 50 ms.
        assert_eq!(timer_interval_ms(8820, 44_100), 50);
        // Degenerate sizes never produce a zero (non-periodic) timer.
        assert_eq!(timer_interval_ms(1, 192_000), 1);
        assert_eq!(timer_interval_ms(8, 44_100), 1);
    }

    #[test]
    fn init_attempts_insert_ms_aligned_period_before_whole_buffer_fallback() {
        // 96 kHz defaults: period 229688 hns (22.97 ms) is fractional.
        let attempts = init_attempts(918_750, 229_688, 918_750);
        assert_eq!(
            attempts,
            vec![
                (918_750, 229_688),
                (918_750, 230_000), // rounded up to whole ms
                (918_750, 918_750), // whole-buffer last resort
            ]
        );
        // An already ms-aligned period (44.1 kHz: 50 ms exactly) needs no retry.
        let attempts = init_attempts(2_000_000, 500_000, 2_000_000);
        assert_eq!(attempts, vec![(2_000_000, 500_000), (2_000_000, 2_000_000)]);
        // A 128-byte-aligned larger buffer is the final fallback.
        let attempts = init_attempts(918_750, 918_750, 920_000);
        assert_eq!(attempts, vec![(918_750, 918_750), (920_000, 920_000)]);
    }

    /// Manual smoke test against real hardware:
    /// `cargo test -p sointty-output-wasapi -- --ignored`
    #[test]
    #[ignore = "requires a real WASAPI render endpoint"]
    fn renders_one_second_of_silence_on_default_device() {
        let mut output = WasapiOutput::new(&"default".to_owned()).unwrap();
        let input = StreamSpec {
            rate_hz: 44_100,
            layout: ChannelLayout::discrete(2),
            encoding: SampleEncoding::S16,
        };
        let spec = output
            .configure(&input, BufferConfig::default_for_rate(44_100))
            .unwrap();
        let frame_bytes = spec.bytes_per_frame();
        let (mut producer, consumer) =
            rtrb::RingBuffer::<u8>::new(frame_bytes * 44_100);
        let counters = Arc::new(OutputCounters::default());
        output.start(consumer, counters.clone()).unwrap();

        let silence = vec![0_u8; frame_bytes * 44_100];
        let mut written = 0;
        while written < silence.len() {
            let (pushed, _) = producer.push_partial_slice(&silence[written..]);
            written += pushed.len();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        output.stop().unwrap();

        assert!(!counters.fault(), "render thread faulted");
        assert!(
            counters.played_frames() >= 44_100,
            "expected at least 1s of frames, got {}",
            counters.played_frames()
        );
    }
}
