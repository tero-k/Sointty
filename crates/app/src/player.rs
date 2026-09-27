use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rand::{Rng, seq::SliceRandom};
use sointty_core::{
    AudioOutput, BufferConfig, Decoder, DecodedBlock, DecodedPcm, DeviceFormat, DeviceId,
    DopPacker, OutputCounters, OutputSpec, PlayerCommand, PlayerError, PlayerEvent, QueueEntry,
    SampleEncoding, StreamSpec, TrackId, TrackTags, cd_frames_to_samples, pack_exact, pack_native_dsd,
};
use sointty_source::StallState;

/// Default time a source may stay stalled before the coordinator stops the
/// output and reports the starvation as an explicit underrun.
/// (Used by the Linux `main` wiring; unit tests pass explicit timeouts.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the coordinator polls the current track's stall handle.
const STALL_POLL: Duration = Duration::from_millis(100);

/// How long the decoder worker waits on its message channel while a pump is
/// in flight, so commands stay prompt even with a full ring.
const WORKER_WAIT: Duration = Duration::from_millis(5);

/// Grace period after an interrupted-looking decode error during which the
/// worker waits for the commanded transition that caused the interruption.
const ABORT_GRACE: Duration = Duration::from_millis(50);

/// A decoder handed back by a [`DecoderFactory`], together with the stall
/// handle of its source (if the source can stall, e.g. a mounted share).
pub struct OpenedDecoder {
    pub decoder: Box<dyn sointty_core::Decoder>,
    pub stall: Option<Arc<StallState>>,
}

pub type DecoderFactory = Box<dyn Fn(&Path) -> Result<OpenedDecoder, PlayerError> + Send>;

#[derive(Clone)]
struct QueuedTrack {
    id: TrackId,
    path: PathBuf,
    cue_range: Option<sointty_core::CueRange>,
    tags: TrackTags,
}

struct PreparedNext {
    track: QueuedTrack,
    decoder: Box<dyn Decoder>,
    stall: Option<Arc<StallState>>,
}

/// How decoded blocks become wire bytes for the current output. DoP keeps a
/// persistent [`DopPacker`] so the marker phase survives block boundaries
/// and seamless same-spec track swaps; every reconfigure/seek starts fresh.
enum Packing {
    Pcm,
    /// Explicitly opted-in, non-bit-perfect conversion on the decoder thread.
    FloatToInt,
    Dop(DopPacker),
    /// Native DSD groups consecutive time bytes per channel into ALSA slots.
    NativeDsd { channels: u16, slot_bytes: usize },
}

impl Packing {
    fn for_output(output: &OutputSpec, converted: bool) -> Self {
        if converted {
            return Self::FloatToInt;
        }
        match output.format {
            DeviceFormat::Dop24 => Self::Dop(DopPacker::new(output.layout.channels)),
            DeviceFormat::DsdU8 | DeviceFormat::DsdU16Le | DeviceFormat::DsdU32Le => {
                Self::NativeDsd {
                    channels: output.layout.channels,
                    slot_bytes: output.format.bytes_per_sample(),
                }
            }
            _ => Self::Pcm,
        }
    }

    fn converted(&self) -> bool {
        matches!(self, Self::FloatToInt)
    }
}

struct Playback {
    decoder: Box<dyn Decoder>,
    producer: rtrb::Producer<u8>,
    output: OutputSpec,
    /// Timing used for this stream (per-rate defaults plus any overrides).
    buffers: BufferConfig,
    written_frames: u64,
    /// Absolute source frame the current range starts at (0 for whole-file
    /// tracks); seeks add the requested range-relative frame to this.
    start_sample: u64,
    /// Span length in frames counted from the range start, `None` = source
    /// EOF. `span_written` reaching this value ends the track.
    end_frames: Option<u64>,
    /// Frames written since the current range started; resets on seek and on
    /// a seamless track swap, while `written_frames` keeps counting.
    span_written: u64,
    /// One packed decoder block may exceed ring capacity. Keep its unwritten
    /// suffix until the consumer makes room; never drop a decoded block.
    scratch: Vec<u8>,
    pending_offset: usize,
    pending_frames: u64,
    packing: Packing,
}

/// Pack one decoded block into `scratch`: PCM through `pack_exact`, DoP
/// through the persistent marker packer, or native DSD into per-channel slots.
/// `pack_exact` is never used for DSD. Free function so the decoder's
/// borrowed block and the output scratch can be accessed disjointly.
fn pack_block(
    packing: &mut Packing,
    output: &OutputSpec,
    scratch: &mut Vec<u8>,
    block: DecodedBlock<'_>,
) -> Result<(), PlayerError> {
    match packing {
        Packing::Pcm => pack_exact(block, output, scratch),
        Packing::FloatToInt => pack_float_to_int(block, output, scratch),
        Packing::Dop(packer) => {
            let DecodedPcm::Dsd(bytes) = &block.pcm else {
                return Err(PlayerError::Decode);
            };
            packer.pack(bytes, scratch)
        }
        Packing::NativeDsd { channels, slot_bytes } => {
            let DecodedPcm::Dsd(bytes) = &block.pcm else {
                return Err(PlayerError::Decode);
            };
            pack_native_dsd(bytes, *channels, *slot_bytes, scratch)
        }
    }
}

/// Compatibility-only conversion. This runs on the decoder worker, never on
/// the render thread. Round to nearest-even, clip to the signed integer range,
/// and reject non-finite decoder samples rather than emitting invalid audio.
fn quantize_float(sample: f32, bits: u32) -> Result<i32, PlayerError> {
    if !sample.is_finite() {
        return Err(PlayerError::Decode);
    }
    let scale = (1_u64 << (bits - 1)) as f64;
    Ok((f64::from(sample) * scale)
        .round_ties_even()
        .clamp(-scale, scale - 1.0) as i32)
}

fn pack_float_to_int(
    block: DecodedBlock<'_>,
    output: &OutputSpec,
    scratch: &mut Vec<u8>,
) -> Result<(), PlayerError> {
    let spec = block.spec.pcm().ok_or(PlayerError::Decode)?;
    let DecodedPcm::F32(samples) = block.pcm else {
        return Err(PlayerError::Decode);
    };
    let (bits, valid_bits) = match output.format {
        DeviceFormat::S16Le => (16, 16),
        DeviceFormat::S24_3Le | DeviceFormat::S24In32Low | DeviceFormat::S24In32High => (24, 24),
        DeviceFormat::S32Le => (32, 32),
        _ => return Err(PlayerError::Decode),
    };
    if spec.encoding != SampleEncoding::F32
        || output.rate_hz != spec.rate_hz
        || output.layout != spec.layout
        || output.valid_bits != valid_bits
        || samples.len() != block.frames as usize * spec.layout.channels as usize
    {
        return Err(PlayerError::Decode);
    }
    scratch.clear();
    scratch.resize(samples.len() * output.format.bytes_per_sample(), 0);
    for (sample, bytes) in samples
        .iter()
        .zip(scratch.chunks_exact_mut(output.format.bytes_per_sample()))
    {
        let value = quantize_float(*sample, bits)?;
        match output.format {
            DeviceFormat::S16Le => bytes.copy_from_slice(&(value as i16).to_le_bytes()),
            DeviceFormat::S24_3Le => bytes.copy_from_slice(&value.to_le_bytes()[..3]),
            DeviceFormat::S24In32Low => bytes.copy_from_slice(&value.to_le_bytes()),
            DeviceFormat::S24In32High => bytes.copy_from_slice(&(value << 8).to_le_bytes()),
            DeviceFormat::S32Le => bytes.copy_from_slice(&value.to_le_bytes()),
            _ => unreachable!("integer formats validated above"),
        }
    }
    Ok(())
}

/// A playback the coordinator suspended after a stall timeout: the decoder
/// is kept so the track can resume at the frame the output had consumed.
struct Suspended {
    playback: Playback,
    resume_frame: u64,
}

enum Pump {
    Active,
    Waiting,
    Finished,
}

/// Coordinator -> decoder worker messages. `StallStop`/`Resume` are
/// synthesized by the coordinator's stall monitor, not user commands.
enum WorkerMsg {
    Enqueue(QueueEntry),
    SetShuffle(bool),
    Play,
    Pause,
    Stop,
    Next,
    SeekFrame(u64),
    SelectDevice(DeviceId),
    SetFloatToInt(bool),
    SetTiming {
        period_frames: Option<u32>,
        buffer_frames: Option<u32>,
    },
    StallStop,
    Resume,
    Quit,
}

/// Shared snapshot of the worker's current track, read by the coordinator
/// for cancellation and stall monitoring.
#[derive(Default)]
struct TrackState {
    track: Option<TrackId>,
    stall: Option<Arc<StallState>>,
}

/// One continuous `is_stalled() == true` period of the current source.
struct StallEpisode {
    started: Instant,
    /// `StallStop` already sent to the worker; the episode ends with a
    /// `Resume` once data flows again.
    stopped: bool,
}

type OutputFactory<O> = Box<dyn FnMut(DeviceId) -> Result<O, PlayerError> + Send>;

/// Frames left to write in the current CUE span (`None` for whole-file
/// tracks, i.e. unlimited).
fn span_remaining(playback: &Playback) -> Option<u64> {
    playback
        .end_frames
        .map(|end| end.saturating_sub(playback.span_written))
}
/// Bound an advertised file length to the active CUE span. Without a file
/// length, an explicit CUE end still supplies an exact progress endpoint.
fn playback_total(playback: &Playback) -> Option<u64> {
    let file_remaining = playback.decoder.total_frames()
        .map(|total| total.saturating_sub(playback.start_sample));
    match playback.end_frames {
        Some(end) => Some(file_remaining.map_or(end, |remaining| remaining.min(end))),
        None => file_remaining,
    }
}


/// Push as many whole wire frames as the ring currently accepts. Returns
/// `true` when the complete decoded block has entered the ring.
fn push_available(playback: &mut Playback) -> Result<bool, PlayerError> {
    let frame_bytes = playback.output.bytes_per_frame();
    if playback.scratch.len() % frame_bytes != 0 {
        return Err(PlayerError::InvalidInput("decoded block is not wire-frame aligned"));
    }
    let remaining = playback.scratch.len() - playback.pending_offset;
    let bytes = playback.producer.slots().min(remaining) / frame_bytes * frame_bytes;
    if bytes > 0 {
        playback
            .producer
            .push_entire_slice(
                &playback.scratch[playback.pending_offset..playback.pending_offset + bytes],
            )
            .map_err(|_| PlayerError::Output)?;
        playback.pending_offset += bytes;
    }
    if playback.pending_offset != playback.scratch.len() {
        return Ok(false);
    }
    playback.written_frames += playback.pending_frames;
    playback.span_written += playback.pending_frames;
    playback.pending_frames = 0;
    playback.pending_offset = 0;
    playback.scratch.clear();
    Ok(true)
}

/// Wait only while there is no room for another wire frame. A control
/// message interrupts the push; the remaining bytes stay in `Playback`.
fn flush_pending(
    playback: &mut Playback,
    msgs: &Receiver<WorkerMsg>,
    pending: &mut Option<WorkerMsg>,
    counters: &OutputCounters,
) -> Result<bool, PlayerError> {
    loop {
        if push_available(playback)? {
            return Ok(true);
        }
        if counters.fault.load(Ordering::Relaxed) || playback.producer.is_abandoned() {
            return Err(PlayerError::Output);
        }
        match msgs.recv_timeout(WORKER_WAIT) {
            Ok(msg) => {
                *pending = Some(msg);
                return Ok(false);
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err(PlayerError::Output);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Return a block covering exactly `frames` of `block`'s data, borrowing the
/// same samples. Used to cut the last block of a CUE span at its end.
fn truncate_block(block: DecodedBlock<'_>, frames: u32) -> DecodedBlock<'_> {
    if frames >= block.frames {
        return block;
    }
    // CUE ranges apply to PCM only; DSD sources never carry cue ranges.
    let channels = block.spec.pcm().map_or(0, |s| s.layout.channels as usize);
    let samples = frames as usize * channels;
    let pcm = match block.pcm {
        DecodedPcm::I16(samples_ref) => DecodedPcm::I16(&samples_ref[..samples]),
        DecodedPcm::I24(samples_ref) => DecodedPcm::I24(&samples_ref[..samples]),
        DecodedPcm::I32(samples_ref) => DecodedPcm::I32(&samples_ref[..samples]),
        DecodedPcm::F32(samples_ref) => DecodedPcm::F32(&samples_ref[..samples]),
        DecodedPcm::Dsd(_) => return block,
    };
    DecodedBlock::new(block.spec, frames, pcm)
}

/// The decoder worker: owns the queue, decoders, ring producer and output,
/// i.e. everything playback-related. `PlayerEngine::run` spawns it and acts
/// as the coordinator, so a decoder blocked in a mounted-share read never
/// freezes command handling.
struct Worker<O: AudioOutput> {
    queue: VecDeque<QueuedTrack>,
    /// Shuffle applies to unprepared pending tracks; the imminent preopened
    /// decoder remains fixed, preserving the gapless boundary.
    shuffle: bool,
    current: Option<QueuedTrack>,
    /// Track actually audible right now; during a gapless transition this
    /// stays the old track until the boundary `Playing` event fires.
    audible: Option<QueuedTrack>,
    /// Monotonic ID source; never derived from queue contents, so duplicate
    /// paths and re-enqueues always get fresh, increasing IDs.
    next_track_id: TrackId,
    prepared: Option<PreparedNext>,
    boundary: Option<(TrackId, u64)>,
    playback: Option<Playback>,
    suspended: Option<Suspended>,
    /// A message received while the ring was full; handled before pumping.
    pending: Option<WorkerMsg>,
    output: O,
    output_factory: OutputFactory<O>,
    decoder_factory: DecoderFactory,
    device: DeviceId,
    /// DAC timing overrides; `None` means the rate-relative default.
    period_frames: Option<u32>,
    buffer_frames: Option<u32>,
    float_to_int: bool,
    counters: Arc<OutputCounters>,
    events: Sender<PlayerEvent>,
    msgs: Receiver<WorkerMsg>,
    shared: Arc<Mutex<TrackState>>,
}

impl<O: AudioOutput> Worker<O> {
    /// Runs until `Quit` or channel disconnect. Command and playback failures
    /// are reported as `PlayerEvent::Error` and leave the worker stopped but
    /// alive: one bad track must never kill the whole engine.
    fn run(mut self) -> Result<(), PlayerError> {
        loop {
            if let Some(msg) = self.pending.take() {
                if !self.handle_reporting(msg) {
                    return Ok(());
                }
                continue;
            }
            if self.playback.is_none() {
                match self.msgs.recv() {
                    Ok(msg) => {
                        if !self.handle_reporting(msg) {
                            return Ok(());
                        }
                    }
                    Err(_) => return Ok(()),
                }
            } else {
                match self.msgs.recv_timeout(WORKER_WAIT) {
                    Ok(msg) => {
                        if !self.handle_reporting(msg) {
                            return Ok(());
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        match self.pump() {
                            Ok(Pump::Active) | Ok(Pump::Waiting) => {}
                            Ok(Pump::Finished) => {
                                // A short track may have been decoded entirely
                                // during prefill and never emitted a Position.
                                if let (Some(track), Some(playback)) =
                                    (&self.current, &self.playback)
                                {
                                    self.emit(PlayerEvent::Position {
                                        track: track.id,
                                        frame: playback.span_written,
                                    });
                                }
                                if let Err(error) = self.start_next() {
                                    self.report_failure(&error);
                                }
                            }
                            Err(error) => {
                                if self.may_be_cancel_abort(&error)
                                    && let Ok(msg) = self.msgs.recv_timeout(ABORT_GRACE)
                                {
                                    // A commanded transition cancelled a
                                    // blocked source read. Process the command
                                    // instead of reporting a decode failure.
                                    if !self.handle_reporting(msg) {
                                        return Ok(());
                                    }
                                    continue;
                                }
                                self.send_error(&error);
                                if matches!(error, PlayerError::Decode) {
                                    if let Err(error) = self.start_next() {
                                        self.report_failure(&error);
                                    }
                                } else {
                                    self.playback = None;
                                    self.suspended = None;
                                    self.unprepare();
                                    self.boundary = None;
                                    let _ = self.output.stop();
                                    self.clear_shared_stall();
                                    self.current = None;
                                    self.audible = None;
                                    self.emit_queue_changed();
                                }
                            }
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
        }
    }

    /// Only decode-ish errors can be the tail of a cancel: output faults are
    /// reported unconditionally.
    fn may_be_cancel_abort(&self, error: &PlayerError) -> bool {
        matches!(error, PlayerError::Decode | PlayerError::Io(_))
    }

    /// Handle a command, converting failures into an `Error` event plus a
    /// clean stopped state instead of killing the worker: one bad track must
    /// not take down the whole engine (regression: `UnsupportedFormat` on
    /// `Play` froze the player with no error ever shown).
    fn handle_reporting(&mut self, msg: WorkerMsg) -> bool {
        let quit = matches!(msg, WorkerMsg::Quit);
        match self.handle(msg) {
            Ok(keep_going) => keep_going,
            Err(error) => {
                self.report_failure(&error);
                !quit
            }
        }
    }

    /// Stop output, drop all playback state and report `error`. The worker
    /// stays alive and keeps accepting commands. A prepared-but-unplayed
    /// next track returns to the queue front; it was never consumed.
    fn report_failure(&mut self, error: &PlayerError) {
        self.playback = None;
        self.suspended = None;
        self.unprepare();
        self.boundary = None;
        let _ = self.output.stop();
        self.clear_shared_stall();
        self.send_error(error);
        self.current = None;
        self.audible = None;
        self.emit_queue_changed();
    }

    /// Return a canceled prepared track to the queue front. Called on every
    /// transition that discards the prepared decoder without playing it, so
    /// Stop/Pause/Seek/SelectDevice never silently lose a queued track.
    fn unprepare(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            self.queue.push_front(prepared.track);
        }
    }

    /// Snapshot the live queue for the UI: the audible track (which lags
    /// `current` until a gapless boundary actually crosses) plus the pending
    /// FIFO order — a swapped-in `current` awaiting its boundary, then the
    /// prepared next, then the rest of the queue, without duplicates.
    fn emit_queue_changed(&self) {
        let to_item = |track: &QueuedTrack| sointty_core::QueueItem {
            id: track.id,
            entry: QueueEntry {
                path: track.path.clone(),
                cue_range: track.cue_range,
            },
        };
        let audible_id = self.audible.as_ref().map(|track| track.id);
        let mut pending = Vec::new();
        let mut push_unique = |track: &QueuedTrack| {
            if Some(track.id) != audible_id && !pending.iter().any(|item: &sointty_core::QueueItem| item.id == track.id) {
                pending.push(to_item(track));
            }
        };
        if let Some(current) = &self.current {
            push_unique(current);
        }
        if let Some(prepared) = &self.prepared {
            push_unique(&prepared.track);
        }
        for track in &self.queue {
            push_unique(track);
        }
        self.emit(PlayerEvent::QueueChanged {
            audible: self.audible.as_ref().map(to_item),
            pending,
        });
    }
    /// Handle one coordinator message. Returns `Ok(false)` when the worker
    /// should shut down.
    fn handle(&mut self, msg: WorkerMsg) -> Result<bool, PlayerError> {
        let keep_going = match msg {
            WorkerMsg::Enqueue(entry) => {
                self.enqueue(entry);
                self.emit_queue_changed();
                true
            }
            WorkerMsg::SetShuffle(enabled) => {
                if enabled && !self.shuffle {
                    self.queue.make_contiguous().shuffle(&mut rand::thread_rng());
                }
                self.shuffle = enabled;
                self.emit_queue_changed();
                true
            }
            WorkerMsg::Play => {
                if self.playback.is_none() {
                    self.start_next()?;
                }
                true
            }
            WorkerMsg::Pause | WorkerMsg::Stop => {
                self.playback = None;
                self.suspended = None;
                self.unprepare();
                self.boundary = None;
                self.output.stop()?;
                self.clear_shared_stall();
                self.current = None;
                self.audible = None;
                self.emit(PlayerEvent::Paused);
                self.emit_queue_changed();
                true
            }
            WorkerMsg::Next => {
                self.playback = None;
                self.suspended = None;
                self.boundary = None;
                self.clear_shared_stall();
                self.start_next()?;
                true
            }
            WorkerMsg::SeekFrame(frame) => {
                self.seek(frame)?;
                true
            }
            WorkerMsg::SelectDevice(device) => {
                self.playback = None;
                self.suspended = None;
                self.unprepare();
                self.boundary = None;
                self.output.stop()?;
                self.clear_shared_stall();
                // Build the replacement before committing: on factory failure
                // the previous device/output pair stays consistent.
                let output = (self.output_factory)(device.clone())?;
                self.device = device;
                self.output = output;
                self.emit(PlayerEvent::Reconfiguring);
                self.start_next()?;
                true
            }
            WorkerMsg::SetFloatToInt(enabled) => {
                // Applies on the next configure. An already running stream
                // keeps its established format until seek/next/restart.
                self.float_to_int = enabled;
                true
            }
            WorkerMsg::SetTiming {
                period_frames,
                buffer_frames,
            } => {
                // Applies on the next configure, like SetFloatToInt.
                self.period_frames = period_frames;
                self.buffer_frames = buffer_frames;
                true
            }
            WorkerMsg::StallStop => {
                self.suspend_for_stall()?;
                true
            }
            WorkerMsg::Resume => {
                self.resume_after_stall()?;
                true
            }
            WorkerMsg::Quit => {
                self.playback = None;
                self.suspended = None;
                self.prepared = None;
                self.boundary = None;
                self.output.stop()?;
                false
            }
        };
        Ok(keep_going)
    }

    fn enqueue(&mut self, entry: QueueEntry) {
        self.next_track_id += 1;
        let id = self.next_track_id;
        let tags = sointty_decode::tags::read_tags(&entry.path).unwrap_or_default();
        let track = QueuedTrack {
            id,
            path: entry.path,
            cue_range: entry.cue_range,
            tags,
        };
        if self.shuffle {
            let index = rand::thread_rng().gen_range(0..=self.queue.len());
            self.queue.insert(index, track);
        } else {
            self.queue.push_back(track);
        }
    }

    /// Buffer timing for one decoder. PCM derives rate-relative defaults and
    /// applies each present override; with no overrides the default ring
    /// stands, otherwise the ring covers 4 buffers (min 4096 frames). DSD
    /// Auto keeps the fixed safe timing (native/DoP candidate rates differ);
    fn buffers_for(&self, decoder: &dyn Decoder) -> Result<BufferConfig, PlayerError> {
        let buffers = if decoder.dsd_spec().is_some() {
            let period = self.period_frames.unwrap_or(2_205);
            let buffer = self.buffer_frames.unwrap_or(8_820);
            BufferConfig {
                period_frames: period,
                buffer_frames: buffer,
                ring_frames: buffer.saturating_mul(4).max(4096),
            }
        } else {
            let rate = decoder.spec().rate_hz;
            if rate == 0 || rate.div_ceil(10).checked_mul(3).is_none() {
                return Err(PlayerError::InvalidInput("decoded rate is invalid for DAC timing"));
            }
            let default = BufferConfig::default_for_rate(rate);
            let period = self.period_frames.unwrap_or(default.period_frames);
            let buffer = self.buffer_frames.unwrap_or(default.buffer_frames);
            let ring = if self.period_frames.is_none() && self.buffer_frames.is_none() {
                default.ring_frames
            } else {
                default.ring_frames.max(buffer.saturating_mul(4)).max(4096)
            };
            BufferConfig {
                period_frames: period,
                buffer_frames: buffer,
                ring_frames: ring,
            }
        };
        let minimum = buffers.period_frames.checked_mul(2).ok_or(PlayerError::InvalidInput(
            "period frames too large for twice-period buffer",
        ))?;
        if buffers.period_frames == 0 || buffers.buffer_frames == 0 || buffers.buffer_frames < minimum {
            return Err(PlayerError::InvalidInput(
                "buffer frames must be at least twice the positive period frames",
            ));
        }
        Ok(buffers)
    }
    /// Exact F32 is always tried first. Only an explicit compatibility opt-in
    /// permits quantization to an exact-rate/layout integer device format.
    /// Returns the output spec, the conversion flag, and the buffer timing
    /// used, so the caller can size the ring and prefill consistently.
    fn configure_for_decoder(
        &mut self,
        decoder: &dyn Decoder,
    ) -> Result<(OutputSpec, bool, BufferConfig), PlayerError> {
        let buffers = self.buffers_for(decoder)?;
        if let Some(dsd) = decoder.dsd_spec() {
            return self
                .output
                .configure_dsd(&dsd, buffers)
                .map(|output| (output, false, buffers));
        }
        let decoded = decoder.spec();
        let original = match self.output.configure(&decoded, buffers) {
            Ok(output) => return Ok((output, false, buffers)),
            Err(error) if self.float_to_int
                && decoded.encoding == SampleEncoding::F32
                && matches!(error, PlayerError::UnsupportedFormat { .. }) => error,
            Err(error) => return Err(error),
        };
        for encoding in [SampleEncoding::S32, SampleEncoding::S24, SampleEncoding::S16] {
            let integer = StreamSpec { encoding, ..decoded };
            match self.output.configure(&integer, buffers) {
                Ok(output) if output.stream_compatible(&integer) => {
                    return Ok((output, true, buffers));
                }
                Ok(_) => return Err(PlayerError::Output),
                Err(PlayerError::UnsupportedFormat { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Err(original)
    }

    fn start_next(&mut self) -> Result<(), PlayerError> {
        self.playback = None;
        self.suspended = None;
        self.boundary = None;
        self.output.stop()?;
        // A prepared decoder or a freshly opened queue entry may be DSD
        // with a PCM-era CUE range. Report that entry and keep scanning;
        // never convert the range or recurse through a long queue.
        let (track, mut decoder, stall) = loop {
            let (track, decoder, stall) = if let Some(prepared) = self.prepared.take() {
                (prepared.track, prepared.decoder, prepared.stall)
            } else {
                let Some(track) = self.queue.pop_front() else {
                    self.current = None;
                    self.audible = None;
                    self.emit(PlayerEvent::EndOfQueue);
                    self.emit_queue_changed();
                    return Ok(());
                };
                let opened = match (self.decoder_factory)(&track.path) {
                    Ok(opened) => opened,
                    Err(error) => {
                        // Attribute the failure to the track that could not be
                        // opened so the Error event names it.
                        self.current = Some(track);
                        return Err(error);
                    }
                };
                (track, opened.decoder, opened.stall)
            };
            if decoder.dsd_spec().is_some() && track.cue_range.is_some() {
                self.emit(PlayerEvent::Error {
                    track: Some(track.id),
                    kind: PlayerError::InvalidInput("CUE ranges unsupported for DSD"),
                });
                continue;
            }
            break (track, decoder, stall);
        };
        let dsd = decoder.dsd_spec();
        let (start_sample, end_frames) = if dsd.is_some() {
            (0, None)
        } else {
            Self::apply_cue_range(&mut decoder, &track)?
        };
        self.current = Some(track.clone());
        self.publish_track(track.id, stall);
        self.emit(PlayerEvent::Reconfiguring);
        let (output_spec, converted, buffers) = self.configure_for_decoder(decoder.as_ref())?;
        let (producer, consumer) =
            rtrb::RingBuffer::new(Self::ring_bytes(buffers, output_spec.bytes_per_frame()));
        let mut playback = Playback {
            decoder,
            producer,
            output: output_spec.clone(),
            buffers,
            written_frames: 0,
            start_sample,
            end_frames,
            span_written: 0,
            scratch: Vec::new(),
            pending_offset: 0,
            pending_frames: 0,
            packing: Packing::for_output(&output_spec, converted),
        };
        self.counters = Arc::new(OutputCounters::default());
        self.prefill(&mut playback)?;
        self.output.start(consumer, Arc::clone(&self.counters))?;
        self.emit(PlayerEvent::Playing {
            track: track.id,
            output: output_spec,
            converted,
        });
        self.emit(PlayerEvent::Tags {
            track: track.id,
            tags: track.tags.clone(),
        });
        self.emit(PlayerEvent::Duration {
            track: track.id,
            total_frames: playback_total(&playback),
        });
        self.audible = Some(track);
        self.playback = Some(playback);
        self.preopen_next();
        self.emit_queue_changed();
        Ok(())
    }

    fn seek(&mut self, frame: u64) -> Result<(), PlayerError> {
        self.unprepare();
        self.emit_queue_changed();
        self.boundary = None;
        let Some(playback) = self.playback.take() else {
            return Ok(());
        };
        self.reset_shared_cancel();
        let mut decoder = playback.decoder;
        // `frame` is relative to the range start; clamp so the target stays
        // inside the track's span.
        let mut target = playback.start_sample + frame;
        if let Some(end_frames) = playback.end_frames {
            target = target.min(playback.start_sample + end_frames);
        }
        decoder.seek_to_frame(target)?;
        self.output.stop()?;
        // `seek_to_frame` counts per-channel DSD bytes for DSD decoders, so
        // the seek target needs no unit conversion.
        let (output_spec, converted, buffers) = self.configure_for_decoder(decoder.as_ref())?;
        let (producer, consumer) =
            rtrb::RingBuffer::new(Self::ring_bytes(buffers, output_spec.bytes_per_frame()));
        let packing = Packing::for_output(&output_spec, converted);
        let mut playback = Playback {
            decoder,
            producer,
            output: output_spec,
            buffers,
            written_frames: 0,
            start_sample: playback.start_sample,
            end_frames: playback.end_frames,
            span_written: 0,
            scratch: Vec::new(),
            pending_offset: 0,
            pending_frames: 0,
            packing,
        };
        self.counters = Arc::new(OutputCounters::default());
        self.prefill(&mut playback)?;
        self.output.start(consumer, Arc::clone(&self.counters))?;
        self.playback = Some(playback);
        if let Some(track) = &self.current {
            self.emit(PlayerEvent::Position {
                track: track.id,
                frame,
            });
        }
        Ok(())
    }

    /// Starvation timeout fired: stop the output and park the playback. The
    /// resume position is the frame count the output had actually consumed,
    /// so resuming re-seeks there (range-aware) and never masks the gap.
    fn suspend_for_stall(&mut self) -> Result<(), PlayerError> {
        let Some(playback) = self.playback.take() else {
            return Ok(());
        };
        let resume_frame = self.counters.played_frames.load(Ordering::Relaxed);
        self.output.stop()?;
        if let Some(track) = &self.current {
            self.emit(PlayerEvent::Underrun { track: track.id });
        }
        self.suspended = Some(Suspended {
            playback,
            resume_frame,
        });
        Ok(())
    }

    /// Starvation ended: re-seek to the preserved frame on a fresh ring and
    /// restart the output, keeping the track resumable.
    fn resume_after_stall(&mut self) -> Result<(), PlayerError> {
        let Some(suspended) = self.suspended.take() else {
            return Ok(());
        };
        self.reset_shared_cancel();
        let Suspended {
            playback: old,
            resume_frame,
        } = suspended;
        let mut decoder = old.decoder;
        let mut target = old.start_sample + resume_frame;
        if let Some(end_frames) = old.end_frames {
            target = target.min(old.start_sample + end_frames);
        }
        decoder.seek_to_frame(target)?;
        self.emit(PlayerEvent::Reconfiguring);
        let (output_spec, converted, buffers) = self.configure_for_decoder(decoder.as_ref())?;
        let (producer, consumer) =
            rtrb::RingBuffer::new(Self::ring_bytes(buffers, output_spec.bytes_per_frame()));
        let mut playback = Playback {
            decoder,
            producer,
            output: output_spec.clone(),
            buffers,
            written_frames: resume_frame,
            start_sample: old.start_sample,
            end_frames: old.end_frames,
            span_written: resume_frame,
            scratch: Vec::new(),
            pending_offset: 0,
            pending_frames: 0,
            packing: Packing::for_output(&output_spec, converted),
        };
        self.counters = Arc::new(OutputCounters::default());
        self.prefill(&mut playback)?;
        self.output.start(consumer, Arc::clone(&self.counters))?;
        self.playback = Some(playback);
        if let Some(track) = &self.current {
            self.emit(PlayerEvent::Playing {
                track: track.id,
                output: output_spec,
                converted,
            });
            self.emit(PlayerEvent::Duration {
                track: track.id,
                total_frames: self.playback.as_ref().and_then(playback_total),
            });
        }
        Ok(())
    }

    fn preopen_next(&mut self) {
        if self.prepared.is_some() {
            return;
        }
        let mut changed = false;
        for _ in 0..2 {
            let Some(track) = self.queue.pop_front() else {
                break;
            };
            changed = true;
            match (self.decoder_factory)(&track.path) {
                Ok(opened) => {
                    self.prepared = Some(PreparedNext {
                        track,
                        decoder: opened.decoder,
                        stall: opened.stall,
                    });
                    break;
                }
                Err(kind) => self.emit(PlayerEvent::Error {
                    track: Some(track.id),
                    kind,
                }),
            }
        }
        if changed {
            self.emit_queue_changed();
        }
    }

    /// Decode and buffer frames before the output starts. If a decoded block
    /// is larger than the ring, retain its suffix for `pump` to push after
    /// startup; never silently lose that block.
    fn prefill(&self, playback: &mut Playback) -> Result<(), PlayerError> {
        let prefill_frames = (2 * playback.buffers.period_frames as u64)
            .min(playback.buffers.ring_frames as u64)
            * playback.output.format.source_frames_per_wire_frame();
        while playback.written_frames < prefill_frames {
            let remaining = span_remaining(playback);
            if matches!(remaining, Some(0)) {
                break;
            }
            let Some(block) = playback.decoder.next_block()? else {
                break;
            };
            let block = match remaining {
                Some(left) if block.frames as u64 > left => truncate_block(block, left as u32),
                _ => block,
            };
            playback.pending_frames = block.frames as u64;
            pack_block(
                &mut playback.packing,
                &playback.output,
                &mut playback.scratch,
                block,
            )?;
            if !push_available(playback)? {
                break;
            }
        }
        Ok(())
    }

    /// A swapped-in track is only announced once the old track's frames
    /// have actually been consumed past the boundary.
    fn emit_pending_boundary(&mut self) {
        let Some((track, boundary)) = self.boundary else {
            return;
        };
        if self.counters.played_frames.load(Ordering::Relaxed) < boundary {
            return;
        }
        self.boundary = None;
        let tags = self
            .current
            .as_ref()
            .map(|track| track.tags.clone())
            .unwrap_or_default();
        self.emit(PlayerEvent::Tags { track, tags });
        if let Some(playback) = &self.playback {
            self.emit(PlayerEvent::Playing {
                track,
                output: playback.output.clone(),
                converted: playback.packing.converted(),
            });
            self.emit(PlayerEvent::Duration {
                track,
                total_frames: playback_total(playback),
            });
        }
        // The boundary Playing event is what makes the swapped-in track
        // audible; refresh the queue snapshot only now.
        self.audible = self.current.clone();
        self.emit_queue_changed();
    }


    fn pump(&mut self) -> Result<Pump, PlayerError> {
        if self.counters.fault.load(Ordering::Relaxed) {
            return Err(PlayerError::Output);
        }
        if self.counters.xruns.swap(0, Ordering::Relaxed) > 0 {
            if let Some(track) = &self.current {
                self.emit(PlayerEvent::Underrun { track: track.id });
            }
        }
        self.emit_pending_boundary();

        let track_id = self.current.as_ref().map(|track| track.id);
        let mut position = None;
        let result = {
            let Some(playback) = self.playback.as_mut() else {
                return Ok(Pump::Finished);
            };
            if !playback.scratch.is_empty() {
                if !flush_pending(playback, &self.msgs, &mut self.pending, &self.counters)? {
                    return Ok(Pump::Waiting);
                }
                position = Some(playback.written_frames);
                Pump::Active
            } else {
                let remaining = span_remaining(playback);
                let block = if matches!(remaining, Some(0)) {
                    None
                } else {
                    playback.decoder.next_block()?
                };
                match block {
                    Some(block) => {
                        let block = match remaining {
                            Some(left) if block.frames as u64 > left => {
                                truncate_block(block, left as u32)
                            }
                            _ => block,
                        };
                        playback.pending_frames = block.frames as u64;
                        pack_block(
                            &mut playback.packing,
                            &playback.output,
                            &mut playback.scratch,
                            block,
                        )?;
                        if !flush_pending(playback, &self.msgs, &mut self.pending, &self.counters)? {
                            return Ok(Pump::Waiting);
                        }
                        position = Some(playback.written_frames);
                        Pump::Active
                    }
                    None => self.end_of_track()?,
                }
            }
        };
        if let (Some(track), Some(frame)) = (track_id, position) {
            self.emit(PlayerEvent::Position { track, frame });
        }
        Ok(result)
    }

    /// End-of-track transition shared by decoder EOF and a fully written CUE
    /// span: wait for the output to drain, then seamlessly swap in the
    /// prepared decoder when the stream spec matches, or finish the track.
    fn end_of_track(&mut self) -> Result<Pump, PlayerError> {
        let Some(playback) = self.playback.as_mut() else {
            return Ok(Pump::Finished);
        };
        if self.counters.played_frames.load(Ordering::Relaxed) < playback.written_frames {
            std::thread::sleep(Duration::from_millis(1));
            return Ok(Pump::Waiting);
        }
        if let Some(mut prepared) = self.prepared.take() {
            // Seamless only when the full decoded spec matches: PCM specs as
            // before; DSD on both bit rate and layout. A DSD<->PCM boundary
            // or a DSD rate change takes the reconfigure path below.
            let same_spec = match (
                playback.decoder.dsd_spec(),
                prepared.decoder.dsd_spec(),
            ) {
                (Some(active), Some(next)) => active == next,
                (None, None) => playback.decoder.spec() == prepared.decoder.spec(),
                _ => false,
            };
            if same_spec && (self.float_to_int || !playback.packing.converted()) {
                // CUE never applies to DSD; hand the track to start_next,
                // which reports the error and skips it.
                if prepared.decoder.dsd_spec().is_some() && prepared.track.cue_range.is_some() {
                    self.prepared = Some(prepared);
                    return Ok(Pump::Finished);
                }
                let (start_sample, end_frames) =
                    Self::apply_cue_range(&mut prepared.decoder, &prepared.track)?;
                // The decoder alone is swapped: the ring, output spec and
                // packing state persist, so a DoP marker phase started in the
                // old track continues uninterrupted into the new one.
                let boundary = playback.written_frames;
                playback.decoder = prepared.decoder;
                playback.start_sample = start_sample;
                playback.end_frames = end_frames;
                playback.span_written = 0;
                self.current = Some(prepared.track.clone());
                self.publish_track(prepared.track.id, prepared.stall);
                self.boundary = Some((prepared.track.id, boundary));
                self.preopen_next();
                // The swapped-in track is pending until its boundary
                // `Playing` fires; show it in the queue snapshot now.
                self.emit_queue_changed();
                return Ok(Pump::Active);
            }
            self.prepared = Some(prepared);
        }
        Ok(Pump::Finished)
    }

    /// Convert a queued track's CUE range into sample frames at the decoder's
    /// rate, seek the decoder to the range start, and return
    /// `(start_sample, span length in frames)` with `None` meaning source EOF.
    fn apply_cue_range(
        decoder: &mut Box<dyn Decoder>,
        track: &QueuedTrack,
    ) -> Result<(u64, Option<u64>), PlayerError> {
        let Some(range) = track.cue_range else {
            return Ok((0, None));
        };
        let rate_hz = decoder.spec().rate_hz;
        let start_sample = cd_frames_to_samples(range.start_cd, rate_hz);
        if start_sample > 0 {
            decoder.seek_to_frame(start_sample)?;
        }
        let end_frames = range
            .end_cd
            .map(|end_cd| cd_frames_to_samples(end_cd, rate_hz).saturating_sub(start_sample));
        Ok((start_sample, end_frames))
    }

    fn ring_bytes(buffers: BufferConfig, frame_bytes: usize) -> usize {
        buffers.ring_frames.max(1) as usize * frame_bytes
    }

    fn publish_track(&self, id: TrackId, stall: Option<Arc<StallState>>) {
        if let Ok(mut state) = self.shared.lock() {
            state.track = Some(id);
            state.stall = stall;
        }
    }

    fn clear_shared_stall(&self) {
        if let Ok(mut state) = self.shared.lock() {
            state.stall = None;
        }
    }

    fn reset_shared_cancel(&self) {
        let stall = self
            .shared
            .lock()
            .ok()
            .and_then(|state| state.stall.clone());
        if let Some(stall) = stall {
            stall.reset_cancel();
        }
    }

    fn emit(&self, event: PlayerEvent) {
        let _ = self.events.send(event);
    }

    fn send_error(&self, kind: &PlayerError) {
        let track = self.current.as_ref().map(|track| track.id);
        self.emit(PlayerEvent::Error {
            track,
            kind: kind.clone(),
        });
    }
}

pub struct PlayerEngine<O: AudioOutput> {
    output: O,
    output_factory: OutputFactory<O>,
    decoder_factory: DecoderFactory,
    device: DeviceId,
    /// DAC timing overrides; `None` derives rate-relative defaults per track.
    period_frames: Option<u32>,
    buffer_frames: Option<u32>,
    float_to_int: bool,
    events: Sender<PlayerEvent>,
    stall_timeout: Duration,
}

impl<O: AudioOutput + 'static> PlayerEngine<O> {
    pub fn new(
        output: O,
        output_factory: OutputFactory<O>,
        device: DeviceId,
        period_frames: Option<u32>,
        buffer_frames: Option<u32>,
        float_to_int: bool,
        decoder_factory: DecoderFactory,
        events: Sender<PlayerEvent>,
        stall_timeout: Duration,
    ) -> Self {
        Self {
            output,
            output_factory,
            decoder_factory,
            device,
            period_frames,
            buffer_frames,
            float_to_int,
            events,
            stall_timeout,
        }
    }

    /// Run the coordinator loop: forward commands to the decoder worker
    /// (cancelling the current source first when the command must interrupt
    /// a blocked read) and monitor the current source for stalls.
    pub fn run(self, commands: Receiver<PlayerCommand>) -> Result<(), PlayerError> {
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Mutex::new(TrackState::default()));
        let worker = Worker {
            queue: VecDeque::new(),
            shuffle: false,
            current: None,
            audible: None,
            next_track_id: 0,
            prepared: None,
            boundary: None,
            playback: None,
            suspended: None,
            pending: None,
            output: self.output,
            output_factory: self.output_factory,
            decoder_factory: self.decoder_factory,
            device: self.device,
            period_frames: self.period_frames,
            buffer_frames: self.buffer_frames,
            float_to_int: self.float_to_int,
            counters: Arc::new(OutputCounters::default()),
            events: self.events.clone(),
            msgs: msg_rx,
            shared: Arc::clone(&shared),
        };
        let thread = std::thread::spawn(move || worker.run());
        let mut coordinator = Coordinator {
            commands,
            msgs: msg_tx,
            events: self.events,
            shared,
            stall_timeout: self.stall_timeout,
            episode: None,
        };
        coordinator.drive();
        // Reap the worker, forwarding its result (it may already have quit on
        // its own error or a forwarded Quit; an extra Quit is a no-op then).
        let _ = coordinator.msgs.send(WorkerMsg::Quit);
        match thread.join() {
            Ok(result) => result,
            Err(_) => Err(PlayerError::Output),
        }
    }
}

struct Coordinator {
    commands: Receiver<PlayerCommand>,
    msgs: Sender<WorkerMsg>,
    events: Sender<PlayerEvent>,
    shared: Arc<Mutex<TrackState>>,
    stall_timeout: Duration,
    episode: Option<StallEpisode>,
}

impl Coordinator {
    fn drive(&mut self) {
        loop {
            match self.commands.recv_timeout(STALL_POLL) {
                Ok(command) => {
                    if !self.forward(command) {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
            self.stall_tick();
        }
    }

    /// Returns `false` when the loop should exit (Quit forwarded, or the
    /// worker died and its result should surface via `join`).
    fn forward(&mut self, command: PlayerCommand) -> bool {
        let quit = matches!(command, PlayerCommand::Quit);
        let msg = match command {
            PlayerCommand::Enqueue(entry) => WorkerMsg::Enqueue(entry),
            PlayerCommand::SetShuffle(enabled) => WorkerMsg::SetShuffle(enabled),
            PlayerCommand::Play => WorkerMsg::Play,
            PlayerCommand::Pause => {
                self.cancel_current();
                WorkerMsg::Pause
            }
            PlayerCommand::Stop => {
                self.cancel_current();
                WorkerMsg::Stop
            }
            PlayerCommand::Next => {
                self.cancel_current();
                WorkerMsg::Next
            }
            PlayerCommand::SeekFrame(frame) => {
                self.cancel_current();
                WorkerMsg::SeekFrame(frame)
            }
            PlayerCommand::SelectDevice(device) => {
                self.cancel_current();
                WorkerMsg::SelectDevice(device)
            }
            PlayerCommand::SetFloatToInt(enabled) => WorkerMsg::SetFloatToInt(enabled),
            PlayerCommand::SetTiming {
                period_frames,
                buffer_frames,
            } => WorkerMsg::SetTiming {
                period_frames,
                buffer_frames,
            },
            PlayerCommand::Quit => {
                self.cancel_current();
                WorkerMsg::Quit
            }
        };
        if self.msgs.send(msg).is_err() {
            return false;
        }
        !quit
    }

    /// Cancel the current track's source so a read blocked in the kernel
    /// unwinds with `Interrupted` and the worker can process the command
    /// that is about to arrive.
    fn cancel_current(&self) {
        let stall = self
            .shared
            .lock()
            .ok()
            .and_then(|state| state.stall.clone());
        if let Some(stall) = stall {
            stall.cancel();
        }
    }

    /// Poll the current source's stall handle. A stall that clears before
    /// the timeout leaves playback untouched (the `Stalled` event already
    /// disclosed it); a stall persisting past the timeout stops the output
    /// with an explicit underrun and resumes once data flows again.
    fn stall_tick(&mut self) {
        let (track, stall) = match self.shared.lock() {
            Ok(state) => (state.track, state.stall.clone()),
            Err(_) => return,
        };
        let Some(stall) = stall else {
            self.episode = None;
            return;
        };
        if !stall.is_stalled() {
            if let Some(episode) = self.episode.take()
                && episode.stopped
            {
                let _ = self.msgs.send(WorkerMsg::Resume);
            }
            // A brief stall that cleared before the timeout needs no other
            // action: playback never stopped.
            return;
        }
        match &mut self.episode {
            None => {
                self.episode = Some(StallEpisode {
                    started: Instant::now(),
                    stopped: false,
                });
                if let Some(track) = track {
                    let _ = self.events.send(PlayerEvent::Stalled { track });
                }
            }
            Some(episode) => {
                if !episode.stopped && episode.started.elapsed() >= self.stall_timeout {
                    episode.stopped = true;
                    stall.cancel();
                    let _ = self.msgs.send(WorkerMsg::StallStop);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io;
    use std::io::Read as _;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use sointty_core::{
        ChannelLayout, DecodedBlock, DecodedSpec, DecodedPcm, DeviceFormat, DopPacker, DsdSpec,
        SampleEncoding, StreamSpec,
    };

    const FRAME_BYTES: usize = 4; // S16 stereo

    /// What the fake decoder factory builds for a queued path: PCM with a
    /// total frame count, or DSD with a total per-channel DSD byte count.
    #[derive(Clone)]
    enum FakeTrack {
        Pcm(StreamSpec, u64),
        PcmBlock(StreamSpec, u64, u32),
        Dsd(DsdSpec, u64),
    }

    fn test_spec(rate_hz: u32) -> StreamSpec {
        StreamSpec {
            rate_hz,
            layout: ChannelLayout::discrete(2),
            encoding: SampleEncoding::S16,
        }
    }

    fn test_dsd_spec() -> DsdSpec {
        DsdSpec {
            dsd_rate_hz: 2_822_400,
            layout: ChannelLayout::discrete(2),
        }
    }

    /// Per-track DSD byte pattern seed, mirroring the decoder factory.
    fn dsd_seed(path: &Path) -> u8 {
        path.to_string_lossy()
            .bytes()
            .fold(0u8, |acc, byte| acc.wrapping_add(byte))
    }

    /// Rebuild the exact raw DSD byte stream a fake DSD decoder emits:
    /// 512 per-channel-byte blocks of the pattern `(i*31 + seed) as u8`
    /// over in-block byte indexes.
    fn dsd_raw_stream(path: &str, total_per_channel: u64, channels: usize) -> Vec<u8> {
        let seed = dsd_seed(Path::new(path));
        let mut out = Vec::new();
        let mut next = 0u64;
        while next < total_per_channel {
            let frames = (total_per_channel - next).min(512) as usize;
            out.extend((0..frames * channels).map(|i| {
                (i as u8).wrapping_mul(31).wrapping_add(seed)
            }));
            next += frames as u64;
        }
        out
    }

    /// Timing overrides the harness passes. The effective ring follows the
    /// engine policy `max(default ring, 4 * buffer, 4096)`; at the tests'
    /// 44.1 kHz the default ring is 44_100/4 = 11_025 frames.
    fn test_buffers() -> BufferConfig {
        BufferConfig {
            period_frames: 256,
            buffer_frames: 512,
            ring_frames: 11_025,
        }
    }

    /// A fake file whose reads block until `release` is set, then serve
    /// endless zeros. Feeding it through a real `ReadAheadSource` gives the
    /// tests genuine stall/cancellation behaviour.
    struct GatedReader {
        release: Arc<AtomicBool>,
        pos: u64,
        len: u64,
    }

    impl GatedReader {
        fn new(release: Arc<AtomicBool>) -> Self {
            Self {
                release,
                pos: 0,
                len: 1 << 30,
            }
        }
    }

    impl io::Read for GatedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            while !self.release.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.pos = (self.pos + buf.len() as u64).min(self.len);
            buf.fill(0);
            Ok(buf.len())
        }
    }

    impl io::Seek for GatedReader {
        fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
            self.pos = match pos {
                io::SeekFrom::Start(at) => at,
                io::SeekFrom::End(off) => (self.len as i64 + off).max(0) as u64,
                io::SeekFrom::Current(off) => (self.pos as i64 + off).max(0) as u64,
            };
            Ok(self.pos)
        }
    }

    struct FakeDecoder {
        spec: StreamSpec,
        /// `Some` for DSD decoders; then `spec` is a placeholder and every
        /// block carries `DecodedPcm::Dsd` with per-channel byte frames.
        dsd: Option<DsdSpec>,
        total_frames: u64,
        next_frame: u64,
        block_frames: u32,
        silence: Vec<i16>,
        silence_f32: Vec<f32>,
        /// Per-block DSD byte pattern, indexed by in-block byte position.
        dsd_data: Vec<u8>,
        seeks: Arc<Mutex<Vec<u64>>>,
        /// Present on stalled-capable decoders: reads from this block (and
        /// unwind with `Interrupted`) exactly like a real decoder reading a
        /// mounted share through `ReadAheadSource`.
        source: Option<sointty_source::ReadAheadSource>,
    }

    impl Decoder for FakeDecoder {
        fn spec(&self) -> StreamSpec {
            self.spec
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.total_frames)
        }


        fn dsd_spec(&self) -> Option<DsdSpec> {
            self.dsd
        }

        fn next_block(&mut self) -> Result<Option<DecodedBlock<'_>>, PlayerError> {
            if self.next_frame >= self.total_frames {
                return Ok(None);
            }
            if let Some(source) = &mut self.source {
                let mut buf = [0u8; 4096];
                source
                    .read(&mut buf)
                    .map_err(|error| PlayerError::Io(error.kind()))?;
            }
            let frames = (self.total_frames - self.next_frame).min(self.block_frames as u64) as u32;
            self.next_frame += frames as u64;
            if let Some(dsd) = self.dsd {
                let channels = dsd.layout.channels as usize;
                let bytes = frames as usize * channels;
                return Ok(Some(DecodedBlock::new(
                    DecodedSpec::Dsd(dsd),
                    frames,
                    DecodedPcm::Dsd(&self.dsd_data[..bytes]),
                )));
            }
            let channels = self.spec.layout.channels as usize;
            let samples = frames as usize * channels;
            let pcm = if self.spec.encoding == SampleEncoding::F32 {
                DecodedPcm::F32(&self.silence_f32[..samples])
            } else {
                DecodedPcm::I16(&self.silence[..samples])
            };
            Ok(Some(DecodedBlock::pcm_block(self.spec, frames, pcm)))
        }

        fn seek_to_frame(&mut self, frame: u64) -> Result<u64, PlayerError> {
            self.seeks.lock().push(frame);
            self.next_frame = frame.min(self.total_frames);
            Ok(self.next_frame)
        }
    }

    #[derive(Default)]
    struct FakeCalls {
        configures: usize,
        dsd_configures: usize,
        starts: usize,
        stops: usize,
        occupied_at_start: Vec<usize>,
        played_total: u64,
        /// Buffer timing of every configure call, in order.
        buffers_seen: Vec<BufferConfig>,
    }

    struct FakeOutput {
        calls: Arc<Mutex<FakeCalls>>,
        stop_flag: Arc<AtomicBool>,
        drain: Option<std::thread::JoinHandle<()>>,
        /// Scripted result of `configure_dsd`: DoP or native DSD format.
        dsd_mode: DeviceFormat,
        /// Wire bytes per written frame, so `played_frames` tracks the
        /// worker's `written_frames` unit (PCM frames / per-channel DSD
        /// bytes) exactly.
        frame_bytes: Arc<AtomicU64>,
        /// Every byte drained from the ring, in drain order.
        drained: Arc<Mutex<Vec<u8>>>,
    }

    impl FakeOutput {
        fn new(
            calls: Arc<Mutex<FakeCalls>>,
            dsd_mode: DeviceFormat,
            drained: Arc<Mutex<Vec<u8>>>,
        ) -> Self {
            Self {
                calls,
                stop_flag: Arc::new(AtomicBool::new(false)),
                drain: None,
                dsd_mode,
                frame_bytes: Arc::new(AtomicU64::new(FRAME_BYTES as u64)),
                drained,
            }
        }
    }

    impl Drop for FakeOutput {
        fn drop(&mut self) {
            self.stop_flag.store(true, Ordering::Relaxed);
            if let Some(drain) = self.drain.take() {
                let _ = drain.join();
            }
        }
    }

    impl AudioOutput for FakeOutput {
        fn configure(
            &mut self,
            input: &StreamSpec,
            buffers: BufferConfig,
        ) -> Result<OutputSpec, PlayerError> {
            {
                let mut calls = self.calls.lock();
                calls.configures += 1;
                calls.buffers_seen.push(buffers);
            }
            let (format, valid_bits) = match input.encoding {
                SampleEncoding::S16 => (DeviceFormat::S16Le, 16),
                SampleEncoding::S32 => (DeviceFormat::S32Le, 32),
                _ => return Err(PlayerError::UnsupportedFormat {
                    rate_hz: input.rate_hz,
                    channels: input.layout.channels,
                    encoding: input.encoding,
                    reason: "fake output supports only S16/S32",
                }),
            };
            self.frame_bytes
                .store(input.bytes_per_frame() as u64, Ordering::Relaxed);
            Ok(OutputSpec {
                device: "fake".to_owned(),
                rate_hz: input.rate_hz,
                layout: input.layout,
                format,
                valid_bits,
            })
        }

        fn configure_dsd(
            &mut self,
            spec: &DsdSpec,
            buffers: BufferConfig,
        ) -> Result<OutputSpec, PlayerError> {
            {
                let mut calls = self.calls.lock();
                calls.dsd_configures += 1;
                calls.buffers_seen.push(buffers);
            }
            let channels = u64::from(spec.layout.channels);
            // Written frames count per-channel DSD bytes; convert to wire
            // bytes per written frame so played_frames tracks written_frames.
            // DoP: two DSD bytes per 3-byte word per channel (stereo tests);
            // native passthrough: one raw byte per channel.
            let per_frame = match self.dsd_mode {
                DeviceFormat::Dop24 => 3 * channels / 2,
                _ => channels,
            };
            self.frame_bytes.store(per_frame.max(1), Ordering::Relaxed);
            let (rate_hz, valid_bits) = match self.dsd_mode {
                DeviceFormat::Dop24 => (spec.dsd_rate_hz / 16, 24),
                DeviceFormat::DsdU32Le => (spec.dsd_rate_hz / 32, 1),
                _ => unreachable!("tests script Dop24 or DsdU32Le"),
            };
            Ok(OutputSpec {
                device: "fake".to_owned(),
                rate_hz,
                layout: spec.layout,
                format: self.dsd_mode,
                valid_bits,
            })
        }

        fn start(
            &mut self,
            mut consumer: rtrb::Consumer<u8>,
            counters: Arc<OutputCounters>,
        ) -> Result<(), PlayerError> {
            {
                let mut calls = self.calls.lock();
                calls.occupied_at_start.push(consumer.slots());
                calls.starts += 1;
            }
            self.stop_flag.store(false, Ordering::Relaxed);
            let stop_flag = Arc::clone(&self.stop_flag);
            let calls = Arc::clone(&self.calls);
            let frame_bytes = Arc::clone(&self.frame_bytes);
            let drained = Arc::clone(&self.drained);
            self.drain = Some(std::thread::spawn(move || {
                let mut pending_bytes: u64 = 0;
                while !stop_flag.load(Ordering::Relaxed) {
                    match consumer.pop() {
                        Ok(byte) => {
                            drained.lock().push(byte);
                            pending_bytes += 1;
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(2)),
                    }
                    let per = frame_bytes.load(Ordering::Relaxed).max(1);
                    let frames = pending_bytes / per;
                    if frames > 0 {
                        counters.played_frames.fetch_add(frames, Ordering::Relaxed);
                        calls.lock().played_total += frames;
                        pending_bytes -= frames * per;
                    }
                }
            }));
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PlayerError> {
            self.calls.lock().stops += 1;
            self.stop_flag.store(true, Ordering::Relaxed);
            if let Some(drain) = self.drain.take() {
                let _ = drain.join();
            }
            Ok(())
        }
    }

    struct Harness {
        commands: Sender<PlayerCommand>,
        events: Receiver<PlayerEvent>,
        calls: Arc<Mutex<FakeCalls>>,
        seeks: Arc<Mutex<Vec<u64>>>,
        release: Arc<AtomicBool>,
        drained: Arc<Mutex<Vec<u8>>>,
        player: Option<std::thread::JoinHandle<Result<(), PlayerError>>>,
    }

    impl Harness {
        fn join(mut self) -> Result<(), PlayerError> {
            self.player
                .take()
                .expect("engine thread")
                .join()
                .expect("engine thread join")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // Unblock any gated source so read-ahead workers can shut down.
            self.release.store(true, Ordering::Relaxed);
        }
    }

    fn spawn_engine(plan: HashMap<PathBuf, FakeTrack>) -> Harness {
        spawn_engine_full(plan, Duration::from_secs(5), DeviceFormat::Dop24)
    }

    fn spawn_engine_with_timeout(
        plan: HashMap<PathBuf, FakeTrack>,
        stall_timeout: Duration,
    ) -> Harness {
        spawn_engine_full(plan, stall_timeout, DeviceFormat::Dop24)
    }

    fn spawn_engine_dsd(plan: HashMap<PathBuf, FakeTrack>, dsd_mode: DeviceFormat) -> Harness {
        spawn_engine_full(plan, Duration::from_secs(5), dsd_mode)
    }

    fn spawn_engine_full(
        plan: HashMap<PathBuf, FakeTrack>,
        stall_timeout: Duration,
        dsd_mode: DeviceFormat,
    ) -> Harness {
        spawn_engine_timed(
            plan,
            stall_timeout,
            dsd_mode,
            Some(test_buffers().period_frames),
            Some(test_buffers().buffer_frames),
        )
    }

    /// Engine without timing overrides: per-rate Auto defaults.
    fn spawn_engine_auto(plan: HashMap<PathBuf, FakeTrack>) -> Harness {
        spawn_engine_timed(plan, Duration::from_secs(5), DeviceFormat::Dop24, None, None)
    }

    fn spawn_engine_timed(
        plan: HashMap<PathBuf, FakeTrack>,
        stall_timeout: Duration,
        dsd_mode: DeviceFormat,
        period_frames: Option<u32>,
        buffer_frames: Option<u32>,
    ) -> Harness {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let calls = Arc::new(Mutex::new(FakeCalls::default()));
        let seeks = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(AtomicBool::new(true));
        let drained = Arc::new(Mutex::new(Vec::new()));
        let output = FakeOutput::new(Arc::clone(&calls), dsd_mode, Arc::clone(&drained));
        let decoder_factory: DecoderFactory = Box::new({
            let seeks = Arc::clone(&seeks);
            let release = Arc::clone(&release);
            move |path: &Path| {
                let Some(track) = plan.get(path).cloned() else {
                    return Err(PlayerError::Decode);
                };
                let seed = dsd_seed(path);
                let (spec, dsd, total_frames, block_frames, silence, dsd_data) = match track {
                    FakeTrack::Pcm(spec, total) => (
                        spec,
                        None,
                        total,
                        512,
                        vec![0; 512 * spec.layout.channels as usize],
                        Vec::new(),
                    ),
                    FakeTrack::PcmBlock(spec, total, block_frames) => (
                        spec,
                        None,
                        total,
                        block_frames,
                        vec![0; block_frames as usize * spec.layout.channels as usize],
                        Vec::new(),
                    ),
                    FakeTrack::Dsd(dsd, total) => {
                        let channels = dsd.layout.channels as usize;
                        (
                            StreamSpec {
                                rate_hz: 0,
                                layout: dsd.layout,
                                encoding: SampleEncoding::Dsd,
                            },
                            Some(dsd),
                            total,
                            512,
                            Vec::new(),
                            (0..512 * channels)
                                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
                                .collect(),
                        )
                    }
                };
                let (source, stall) =
                    sointty_source::ReadAheadSource::with_reader(
                        GatedReader::new(Arc::clone(&release)),
                        None,
                        1 << 20,
                    );
                Ok(OpenedDecoder {
                    decoder: Box::new(FakeDecoder {
                        spec,
                        dsd,
                        total_frames,
                        next_frame: 0,
                        block_frames,
                        silence,
                        silence_f32: if spec.encoding == SampleEncoding::F32 {
                            vec![0.25; block_frames as usize * spec.layout.channels as usize]
                        } else {
                            Vec::new()
                        },
                        dsd_data,
                        seeks: Arc::clone(&seeks),
                        source: Some(source),
                    }),
                    stall: Some(stall),
                })
            }
        });
        let engine = PlayerEngine::new(
            output,
            Box::new(|_| Err(PlayerError::Output)),
            "fake".to_owned(),
            period_frames,
            buffer_frames,
            false,
            decoder_factory,
            event_tx,
            stall_timeout,
        );
        let player = std::thread::spawn(move || engine.run(command_rx));
        Harness {
            commands: command_tx,
            events: event_rx,
            calls,
            seeks,
            release,
            drained,
            player: Some(player),
        }
    }

    /// Receive the next non-snapshot event. `QueueChanged` snapshots are
    /// frequent and asynchronous; tests that care about them use
    /// [`recv_queue_changed`].
    fn recv_event(events: &Receiver<PlayerEvent>) -> PlayerEvent {
        loop {
            let event = events
                .recv_timeout(Duration::from_secs(5))
                .expect("player event within 5s");
            if !matches!(event, PlayerEvent::QueueChanged { .. }) {
                return event;
            }
        }
    }

    /// Receive the next `QueueChanged` snapshot, skipping anything else.
    fn recv_queue_changed(events: &Receiver<PlayerEvent>) -> (Option<sointty_core::QueueItem>, Vec<sointty_core::QueueItem>) {
        loop {
            let event = events
                .recv_timeout(Duration::from_secs(5))
                .expect("player event within 5s");
            if let PlayerEvent::QueueChanged { audible, pending } = event {
                return (audible, pending);
            }
        }
    }



    /// Receive `QueueChanged` snapshots until `predicate` matches.
    fn recv_queue_until(
        events: &Receiver<PlayerEvent>,
        predicate: impl Fn(&Option<sointty_core::QueueItem>, &[sointty_core::QueueItem]) -> bool,
    ) -> (Option<sointty_core::QueueItem>, Vec<sointty_core::QueueItem>) {
        loop {
            let (audible, pending) = recv_queue_changed(events);
            if predicate(&audible, &pending) {
                return (audible, pending);
            }
        }
    }
    /// Receive events until `predicate` matches; returns the matching event.
    fn recv_until(
        events: &Receiver<PlayerEvent>,
        predicate: impl Fn(&PlayerEvent) -> bool,
    ) -> PlayerEvent {
        for _ in 0..512 {
            let event = recv_event(events);
            if predicate(&event) {
                return event;
            }
        }
        panic!("no matching event within 512 events");
    }

    fn enqueue(harness: &Harness, name: &str) {
        harness
            .commands
            .send(PlayerCommand::Enqueue(PathBuf::from(name).into()))
            .unwrap();
    }

    fn enqueue_ranged(harness: &Harness, name: &str, cue_range: sointty_core::CueRange) {
        harness
            .commands
            .send(PlayerCommand::Enqueue(sointty_core::QueueEntry {
                path: PathBuf::from(name),
                cue_range: Some(cue_range),
            }))
            .unwrap();
    }

    fn count_events(
        events: &Receiver<PlayerEvent>,
        predicate: impl Fn(&PlayerEvent) -> bool,
        window: Duration,
    ) -> usize {
        let mut count = 0;
        let deadline = Instant::now() + window;
        while let Some(timeout) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(timeout) {
                Ok(event) => {
                    if predicate(&event) {
                        count += 1;
                    }
                }
                Err(_) => break,
            }
        }
        count
    }

    #[test]
    fn same_spec_tracks_swap_without_reconfigure() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 1024));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();

        // The engine pops the queue front-first (FIFO): a (id 1), then b (id 2).
        assert!(matches!(
            recv_event(&harness.events),
            PlayerEvent::Reconfiguring
        ));
        let first = recv_event(&harness.events);
        let PlayerEvent::Playing { track: first_id, .. } = first else {
            panic!("expected Playing, got {first:?}");
        };
        let tags = recv_event(&harness.events);
        let PlayerEvent::Tags { track: tags_id, .. } = tags else {
            panic!("expected Tags, got {tags:?}");
        };
        assert_eq!(first_id, tags_id);
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.configures, 1);
            assert_eq!(calls.starts, 1);
        }

        // Track 2 is announced only after track 1's frames are consumed.
        let mut boundary = None;
        let mut preceding = None;
        for _ in 0..64 {
            let event = recv_event(&harness.events);
            match event {
                PlayerEvent::Playing { track, .. } => {
                    boundary = Some(track);
                    break;
                }
                other => preceding = Some(other),
            }
        }
        let second_id = boundary.expect("second Playing event");
        assert_ne!(second_id, first_id);
        match preceding {
            Some(PlayerEvent::Tags { track, .. }) => assert_eq!(track, second_id),
            other => panic!("expected Tags before boundary Playing, got {other:?}"),
        }
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.configures, 1, "same-spec swap must not reconfigure");
            assert_eq!(calls.starts, 1, "same-spec swap must not restart output");
            assert!(
                calls.played_total >= 1024,
                "track 2 announced before track 1 was consumed"
            );
        }

        let mut end = false;
        for _ in 0..128 {
            if matches!(recv_event(&harness.events), PlayerEvent::EndOfQueue) {
                end = true;
                break;
            }
        }
        assert!(end, "expected EndOfQueue");
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.played_total, 1024 + 512, "exact frame accounting");
        }
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn different_rate_track_reconfigures_output() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(48_000), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let first = recv_event(&harness.events);
        assert!(matches!(first, PlayerEvent::Reconfiguring));
        let first = recv_event(&harness.events);
        let PlayerEvent::Playing { track: first_id, output, .. } = first else {
            panic!("expected Playing, got {first:?}");
        };
        assert_eq!(output.rate_hz, 44_100);
        recv_event(&harness.events); // Tags for first track

        let mut second_playing = false;
        for _ in 0..64 {
            match recv_event(&harness.events) {
                PlayerEvent::Playing { track, .. } if track != first_id => {
                    second_playing = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(second_playing, "expected second Playing after rate change");
        recv_event(&harness.events); // Tags for second track
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.configures, 2, "rate change must reconfigure");
            assert_eq!(calls.starts, 2, "rate change must restart output");
            assert!(calls.stops >= 2, "output stopped before reconfigure");
        }
        for _ in 0..64 {
            if matches!(recv_event(&harness.events), PlayerEvent::EndOfQueue) {
                break;
            }
        }
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn seek_replaces_ring_and_reports_position() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 4096));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let mut started = false;
        for _ in 0..16 {
            if matches!(
                recv_event(&harness.events),
                PlayerEvent::Playing { .. }
            ) {
                started = true;
                break;
            }
        }
        assert!(started, "expected first Playing");
        recv_event(&harness.events); // Tags

        harness.commands.send(PlayerCommand::SeekFrame(1000)).unwrap();
        let mut position = None;
        for _ in 0..64 {
            match recv_event(&harness.events) {
                PlayerEvent::Position { track, frame: 1000 } => {
                    position = Some(track);
                    break;
                }
                _ => {}
            }
        }
        let track = position.expect("Position event after seek");
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.configures, 2, "seek must reconfigure");
            assert_eq!(calls.starts, 2, "seek must restart output");
            assert!(calls.stops >= 2);
        }

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
        let _ = track;
    }

    #[test]
    fn start_prefills_ring_before_output_start() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 8192));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let mut started = false;
        for _ in 0..16 {
            if matches!(recv_event(&harness.events), PlayerEvent::Playing { .. }) {
                started = true;
                break;
            }
        }
        assert!(started, "expected Playing");
        recv_event(&harness.events); // Tags
        {
            let calls = harness.calls.lock();
            let occupied = calls.occupied_at_start[0];
            let prefill_frames = (2 * test_buffers().period_frames as u64)
                .min(test_buffers().ring_frames as u64);
            assert!(
                occupied as u64 >= prefill_frames * FRAME_BYTES as u64,
                "ring held {occupied} bytes at start, expected at least {}",
                prefill_frames * FRAME_BYTES as u64
            );
        }
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn prefilled_short_track_reports_final_position() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("short.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "short.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        let mut final_position = None;
        loop {
            match recv_event(&harness.events) {
                PlayerEvent::Position { track: 1, frame } => final_position = Some(frame),
                PlayerEvent::EndOfQueue => break,
                _ => {}
            }
        }
        assert_eq!(final_position, Some(512));
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn decoded_block_larger_than_ring_is_not_dropped_or_stuck() {
        let mut plan = HashMap::new();
        // The block must exceed the effective ring (11_025 frames at 44.1
        // kHz with the harness timing overrides).
        plan.insert(
            PathBuf::from("large.flac"),
            FakeTrack::PcmBlock(test_spec(44_100), 16_384, 16_384),
        );
        let harness = spawn_engine(plan);
        enqueue(&harness, "large.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::EndOfQueue));
        let calls = harness.calls.lock();
        assert_eq!(calls.played_total, 16_384, "entire block reached the output");
        assert!(calls.occupied_at_start[0] >= test_buffers().ring_frames as usize * FRAME_BYTES);
        drop(calls);
        assert_eq!(harness.drained.lock().len(), 16_384 * FRAME_BYTES);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn ranged_track_plays_exact_span() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 588 * 10));
        let harness = spawn_engine(plan);
        // cd 2..4 at 44.1 kHz = samples 1176..2352, span 1176 = 2x512 + 152,
        // so the last 512-frame block must be truncated.
        enqueue_ranged(
            &harness,
            "a.flac",
            sointty_core::CueRange {
                start_cd: 2,
                end_cd: Some(4),
            },
        );
        harness.commands.send(PlayerCommand::Play).unwrap();

        let mut ended = false;
        for _ in 0..256 {
            if matches!(recv_event(&harness.events), PlayerEvent::EndOfQueue) {
                ended = true;
                break;
            }
        }
        assert!(ended, "expected EndOfQueue");
        assert!(
            harness.seeks.lock().contains(&1176),
            "decoder must be seeked to the range start sample"
        );
        {
            let calls = harness.calls.lock();
            assert_eq!(
                calls.played_total, 1176,
                "ranged track must emit exactly its span, no more, no fewer"
            );
        }
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn adjacent_cue_ranges_are_gapless() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 588 * 10));
        let harness = spawn_engine(plan);
        enqueue_ranged(
            &harness,
            "a.flac",
            sointty_core::CueRange {
                start_cd: 0,
                end_cd: Some(2),
            },
        );
        enqueue_ranged(
            &harness,
            "a.flac",
            sointty_core::CueRange {
                start_cd: 2,
                end_cd: Some(4),
            },
        );
        harness.commands.send(PlayerCommand::Play).unwrap();

        let first = recv_event(&harness.events);
        assert!(matches!(first, PlayerEvent::Reconfiguring));
        let first = recv_event(&harness.events);
        let PlayerEvent::Playing { track: first_id, .. } = first else {
            panic!("expected Playing, got {first:?}");
        };
        assert_eq!(first_id, 1);
        recv_event(&harness.events); // Tags for track 1
        assert!(matches!(
            recv_event(&harness.events),
            PlayerEvent::Duration { track: 1, total_frames: Some(1176) }
        ));

        // Track 2 is announced only after track 1's full span (1176 frames)
        // has been consumed: the seamless boundary sits at the span end.
        // Track 2's own span exhausts immediately after the swap, so Tags,
        // Playing, and EndOfQueue arrive back to back; collect them in one
        // loop.
        let mut second_playing = false;
        let mut second_duration = None;
        let mut ended = false;
        for _ in 0..512 {
            match recv_event(&harness.events) {
                PlayerEvent::Playing { track: 2, .. } => {
                    if !second_playing {
                        second_playing = true;
                        let calls = harness.calls.lock();
                        assert!(
                            calls.played_total >= 1176,
                            "track 2 announced before track 1's span was consumed"
                        );
                        assert_eq!(
                            calls.configures, 1,
                            "same-spec swap must not reconfigure"
                        );
                        assert_eq!(
                            calls.starts, 1,
                            "same-spec swap must not restart output"
                        );
                    }
                }
                PlayerEvent::EndOfQueue => {
                    ended = true;
                    break;
                }
                PlayerEvent::Duration { track: 2, total_frames } => {
                    second_duration = total_frames;
                }
                _ => {}
            }
        }
        assert!(second_playing, "expected gapless Playing for track 2");
        assert_eq!(second_duration, Some(1176));
        assert!(ended, "expected EndOfQueue");
        {
            let calls = harness.calls.lock();
            assert_eq!(
                calls.played_total,
                588 * 4,
                "adjacent ranges must play every frame exactly once"
            );
        }
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn seek_within_range_offsets_by_start() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 588 * 10));
        let harness = spawn_engine(plan);
        enqueue_ranged(
            &harness,
            "a.flac",
            sointty_core::CueRange {
                start_cd: 2,
                end_cd: None,
            },
        );
        harness.commands.send(PlayerCommand::Play).unwrap();

        let mut started = false;
        for _ in 0..16 {
            if matches!(recv_event(&harness.events), PlayerEvent::Playing { .. }) {
                started = true;
                break;
            }
        }
        assert!(started, "expected first Playing");
        recv_event(&harness.events); // Tags
        assert!(
            harness.seeks.lock().contains(&1176),
            "range start seek before playback"
        );

        harness.commands.send(PlayerCommand::SeekFrame(100)).unwrap();
        let mut sought = false;
        for _ in 0..64 {
            match recv_event(&harness.events) {
                PlayerEvent::Position { frame: 100, .. } => {
                    sought = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(sought, "Position event for range-relative frame 100");
        assert!(
            harness.seeks.lock().contains(&1276),
            "decoder seek must be offset by the range start (1176 + 100)"
        );
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn fifo_order() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(48_000), 512));
        plan.insert(PathBuf::from("c.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        enqueue(&harness, "c.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();

        for expected in 1..=3u64 {
            let mut seen = None;
            for _ in 0..64 {
                if let PlayerEvent::Playing { track, .. } = recv_event(&harness.events) {
                    seen = Some(track);
                    break;
                }
            }
            assert_eq!(
                seen,
                Some(expected),
                "FIFO order: tracks must play 1, 2, 3"
            );
        }
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn coordinator_responsive_during_blocked_decode() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 1_000_000));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Tags { .. }));

        // Close the gate: the decoder's next read blocks in the source.
        harness.release.store(false, Ordering::Relaxed);
        // Wait until the coordinator has observed the stall, i.e. the decode
        // is genuinely blocked.
        let stalled = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Stalled { .. })
        });
        assert!(matches!(stalled, PlayerEvent::Stalled { track: 1 }));

        // Stop must interrupt the blocked read via cancel() and produce a
        // Paused event promptly, without releasing the gate.
        let stops_before = harness.calls.lock().stops;
        harness.commands.send(PlayerCommand::Stop).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Paused)
        });
        {
            let calls = harness.calls.lock();
            assert!(
                calls.stops > stops_before,
                "stop must reach the output even while the decode is blocked"
            );
        }
        assert!(
            !harness.release.load(Ordering::Relaxed),
            "the decoder block must be unwound by cancel(), not by the gate"
        );

        // Let the dropped decoder's source worker shut down cleanly.
        harness.release.store(true, Ordering::Relaxed);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn stall_timeout_stops_and_resumes() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 1_000_000));
        let harness = spawn_engine_with_timeout(plan, Duration::from_millis(200));
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Tags { .. }));

        harness.release.store(false, Ordering::Relaxed);
        let stalled = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Stalled { track: 1 })
        });
        assert!(matches!(stalled, PlayerEvent::Stalled { track: 1 }));

        // Past the timeout the output must stop with an explicit underrun.
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Underrun { track: 1 })
        });
        let played = harness.calls.lock().played_total;
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.starts, 1, "one start before the stall");
            assert!(calls.stops >= 2, "stall must stop the output");
        }

        // Clear the stall: the worker must re-seek to the preserved frame
        // and restart the output.
        harness.release.store(true, Ordering::Relaxed);
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        assert_eq!(
            *harness.seeks.lock(),
            vec![played],
            "resume must seek to the frame the output had consumed"
        );
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.starts, 2, "resume must restart the output");
        }

        // Exactly one Stalled event for the episode, even after resuming.
        let stalled_again = count_events(
            &harness.events,
            |event| matches!(event, PlayerEvent::Stalled { .. }),
            Duration::from_millis(500),
        );
        assert_eq!(stalled_again, 0, "no second Stalled after the resume");

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn brief_stall_does_not_stop() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 1_000_000));
        let harness = spawn_engine_with_timeout(plan, Duration::from_millis(400));
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Tags { .. }));

        harness.release.store(false, Ordering::Relaxed);
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Stalled { track: 1 })
        });
        // Clear the stall well before the 400 ms timeout.
        harness.release.store(true, Ordering::Relaxed);

        // No stop/restart and no underrun may follow; playback continues.
        let underruns = count_events(
            &harness.events,
            |event| matches!(event, PlayerEvent::Underrun { .. }),
            Duration::from_secs(1),
        );
        assert_eq!(underruns, 0, "brief stall must not stop playback");
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.starts, 1, "brief stall must not restart output");
            assert_eq!(calls.stops, 1, "brief stall must not stop output");
        }
        // Playback kept flowing after the gate reopened.
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Position { track: 1, .. })
        });

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn next_during_stall_skips() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 1_000_000));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine_with_timeout(plan, Duration::from_millis(400));
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Tags { .. }));

        harness.release.store(false, Ordering::Relaxed);
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Stalled { track: 1 })
        });

        // Next must interrupt the blocked track and start the queued one.
        harness.commands.send(PlayerCommand::Next).unwrap();
        // Wait for the new track's output stop, then reopen the gate so its
        // prefill can read.
        let deadline = Instant::now() + Duration::from_secs(5);
        while harness.calls.lock().stops < 2 {
            assert!(Instant::now() < deadline, "second output stop never happened");
            std::thread::sleep(Duration::from_millis(5));
        }
        harness.release.store(true, Ordering::Relaxed);

        let playing = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        assert!(matches!(playing, PlayerEvent::Playing { track: 2, .. }));
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.starts, 2, "next must start the following track");
            assert!(calls.stops >= 2, "next must stop the stalled track");
        }

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn dsd_track_uses_configure_dsd_and_dop_packing() {
        let mut plan = HashMap::new();
        plan.insert(
            PathBuf::from("a.dsf"),
            FakeTrack::Dsd(test_dsd_spec(), 1024),
        );
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue(&harness, "a.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let playing = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        let PlayerEvent::Playing { output, .. } = playing else {
            unreachable!()
        };
        assert_eq!(output.format, DeviceFormat::Dop24);
        assert_eq!(output.rate_hz, 176_400, "DoP wire rate for DSD64");
        assert_eq!(output.layout.channels, 2);
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Tags { track: 1, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });

        {
            let calls = harness.calls.lock();
            assert_eq!(calls.configures, 0, "PCM configure must never run for DSD");
            assert_eq!(calls.dsd_configures, 1);
            assert_eq!(calls.starts, 1);
        }

        let raw = dsd_raw_stream("a.dsf", 1024, 2);
        let mut expected = Vec::new();
        DopPacker::new(2).pack(&raw, &mut expected).unwrap();
        assert_eq!(
            expected.len(),
            1024 / 2 * 3 * 2,
            "one 3-byte word per 2 DSD bytes per channel"
        );
        let drained = harness.drained.lock();
        assert_eq!(
            *drained, expected,
            "drained wire bytes must equal one DopPacker run over the raw DSD bytes"
        );
        assert!(drained.contains(&0x05), "DoP marker byte must be present");
        drop(drained);

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn dsd_native_slots_preserve_bits_and_channels() {
        let mut plan = HashMap::new();
        plan.insert(
            PathBuf::from("a.dsf"),
            FakeTrack::Dsd(test_dsd_spec(), 1024),
        );
        let harness = spawn_engine_dsd(plan, DeviceFormat::DsdU32Le);
        enqueue(&harness, "a.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let playing = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        let PlayerEvent::Playing { output, .. } = playing else {
            unreachable!()
        };
        assert_eq!(output.format, DeviceFormat::DsdU32Le);
        assert_eq!(output.rate_hz, 88_200, "ALSA-style DSD_U32_LE rate for DSD64");
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Tags { track: 1, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });

        {
            let calls = harness.calls.lock();
            assert_eq!(calls.configures, 0);
            assert_eq!(calls.dsd_configures, 1);
        }
        let mut expected = Vec::new();
        pack_native_dsd(&dsd_raw_stream("a.dsf", 1024, 2), 2, 4, &mut expected).unwrap();
        let drained = harness.drained.lock();
        assert_eq!(*drained, expected, "native DSD slots must preserve every bit and channel");
        drop(drained);

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn dsd_to_pcm_reconfigures() {
        let mut plan = HashMap::new();
        plan.insert(
            PathBuf::from("a.dsf"),
            FakeTrack::Dsd(test_dsd_spec(), 512),
        );
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue(&harness, "a.dsf");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let playing = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        let PlayerEvent::Playing { output, .. } = playing else {
            unreachable!()
        };
        assert_eq!(output.format, DeviceFormat::Dop24);
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Tags { track: 1, .. })
        });

        // A DSD -> PCM boundary must take the reconfigure path, never a
        // seamless swap.
        let playing = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        let PlayerEvent::Playing { output, .. } = playing else {
            unreachable!()
        };
        assert_eq!(output.format, DeviceFormat::S16Le);
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Tags { track: 2, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.dsd_configures, 1);
            assert_eq!(calls.configures, 1);
            assert_eq!(calls.starts, 2, "kind change must restart the output");
        }

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn different_dsd_rate_reconfigures() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.dsf"), FakeTrack::Dsd(test_dsd_spec(), 512));
        plan.insert(
            PathBuf::from("b.dsf"),
            FakeTrack::Dsd(
                DsdSpec {
                    dsd_rate_hz: 5_644_800,
                    ..test_dsd_spec()
                },
                512,
            ),
        );
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue(&harness, "a.dsf");
        enqueue(&harness, "b.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        let playing = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        let PlayerEvent::Playing { output, .. } = playing else {
            unreachable!()
        };
        assert_eq!(output.rate_hz, 352_800, "DoP wire rate for DSD128");
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.dsd_configures, 2, "DSD bit-rate change must reconfigure");
            assert_eq!(calls.starts, 2, "DSD bit-rate change must restart output");
        }

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn identical_dsd_tracks_seamless() {
        let mut plan = HashMap::new();
        // 1002 DSD bytes/channel = 501 DoP frames: odd phase at the
        // boundary, so restarting the marker at 0x05 would be detectable.
        plan.insert(PathBuf::from("a.dsf"), FakeTrack::Dsd(test_dsd_spec(), 1002));
        plan.insert(PathBuf::from("b.dsf"), FakeTrack::Dsd(test_dsd_spec(), 520));
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue(&harness, "a.dsf");
        enqueue(&harness, "b.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Tags { track: 1, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        {
            let calls = harness.calls.lock();
            assert_eq!(
                calls.dsd_configures, 1,
                "identical DSD specs must swap seamlessly"
            );
            assert_eq!(calls.starts, 1, "identical DSD specs must not restart output");
        }
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });

        let mut raw = dsd_raw_stream("a.dsf", 1002, 2);
        raw.extend(dsd_raw_stream("b.dsf", 520, 2));
        let mut expected = Vec::new();
        DopPacker::new(2).pack(&raw, &mut expected).unwrap();
        let drained = harness.drained.lock();
        assert_eq!(
            *drained, expected,
            "DoP marker phase must persist across the seamless boundary"
        );
        drop(drained);

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn dsd_cue_range_errors_and_skips() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.dsf"), FakeTrack::Dsd(test_dsd_spec(), 1000));
        plan.insert(PathBuf::from("b.dsf"), FakeTrack::Dsd(test_dsd_spec(), 512));
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue_ranged(
            &harness,
            "a.dsf",
            sointty_core::CueRange {
                start_cd: 1,
                end_cd: Some(2),
            },
        );
        enqueue(&harness, "b.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        let error = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Error { .. })
        });
        let PlayerEvent::Error { track, kind } = error else {
            unreachable!()
        };
        assert_eq!(track, Some(1));
        assert_eq!(
            kind,
            PlayerError::InvalidInput("CUE ranges unsupported for DSD")
        );
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.dsd_configures, 1, "only the second track configures");
            assert_eq!(calls.configures, 0);
        }

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn prepared_dsd_cue_range_skips_between_tracks() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.dsf"), FakeTrack::Dsd(test_dsd_spec(), 512));
        plan.insert(PathBuf::from("b.dsf"), FakeTrack::Dsd(test_dsd_spec(), 512));
        plan.insert(PathBuf::from("c.dsf"), FakeTrack::Dsd(test_dsd_spec(), 512));
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue(&harness, "a.dsf");
        enqueue_ranged(
            &harness,
            "b.dsf",
            sointty_core::CueRange {
                start_cd: 1,
                end_cd: Some(2),
            },
        );
        enqueue(&harness, "c.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        let error = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Error { track: Some(2), .. })
        });
        let PlayerEvent::Error { kind, .. } = error else {
            unreachable!()
        };
        assert_eq!(
            kind,
            PlayerError::InvalidInput("CUE ranges unsupported for DSD")
        );
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 3, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.dsd_configures, 2, "prepared CUE track must not configure");
            assert_eq!(calls.starts, 2, "third track starts after the rejected CUE track");
        }

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn dsd_seek_reconfigures_with_fresh_dop_packer() {
        let mut plan = HashMap::new();
        plan.insert(
            PathBuf::from("a.dsf"),
            FakeTrack::Dsd(test_dsd_spec(), 1024),
        );
        let harness = spawn_engine_dsd(plan, DeviceFormat::Dop24);
        enqueue(&harness, "a.dsf");
        harness.commands.send(PlayerCommand::Play).unwrap();

        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Tags { track: 1, .. })
        });

        harness.commands.send(PlayerCommand::SeekFrame(512)).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Position { frame: 512, .. })
        });
        {
            let calls = harness.calls.lock();
            assert_eq!(calls.dsd_configures, 2, "seek must reconfigure via configure_dsd");
            assert_eq!(calls.configures, 0);
            assert_eq!(calls.starts, 2, "seek must restart the output");
        }
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::EndOfQueue)
        });

        // Everything drained after the seek must equal a FRESH packer over
        // the bytes from the seek target: the marker phase restarts.
        let raw = dsd_raw_stream("a.dsf", 1024, 2);
        let mut expected = Vec::new();
        DopPacker::new(2).pack(&raw[512 * 2..], &mut expected).unwrap();
        let drained = harness.drained.lock();
        assert!(
            drained.ends_with(&expected),
            "post-seek bytes must come from a fresh DopPacker (marker phase restarted)"
        );
        drop(drained);

        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn unplayable_track_reports_error_and_engine_survives() {
        // Regression: a track that failed to open killed the worker silently
        // — no Error event, every later command ignored, UI frozen until quit.
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("good.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "missing.mp3");
        harness.commands.send(PlayerCommand::Play).unwrap();
        let error = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Error { kind: PlayerError::Decode, .. })
        });
        assert!(
            matches!(error, PlayerEvent::Error { track: Some(1), .. }),
            "error must name the failing track"
        );
        // The engine still accepts commands: a good track plays afterwards.
        enqueue(&harness, "good.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn unsupported_f32_reports_error_and_next_plays() {
        // An integer-only endpoint rejects f32 at configure time. The error
        // must be visible immediately and a subsequent S16 track must play.
        let mut plan = HashMap::new();
        let mut float = test_spec(44_100);
        float.encoding = SampleEncoding::F32;
        plan.insert(PathBuf::from("float.mp3"), FakeTrack::Pcm(float, 512));
        plan.insert(PathBuf::from("good.wav"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "float.mp3");
        enqueue(&harness, "good.wav");
        harness.commands.send(PlayerCommand::Play).unwrap();
        let error = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Error { kind: PlayerError::UnsupportedFormat { .. }, .. })
        });
        assert!(matches!(error, PlayerEvent::Error { track: Some(1), .. }));
        harness.commands.send(PlayerCommand::Next).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, .. })
        });
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn failing_auto_advance_reports_error_and_engine_survives() {
        // Same regression through the automatic queue advance: the good first
        // track ends, advancing into the broken one must report, not die.
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "missing.mp3");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Error { kind: PlayerError::Decode, .. })
        });
        // a.flac may finish before these arrive; the queue can empty and
        // restart track ids, so accept the next Playing whatever its id.
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { .. })
        });
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn float_compatibility_quantizes_and_preserves_channel_order() {
        let spec = StreamSpec { encoding: SampleEncoding::F32, ..test_spec(44_100) };
        let output = OutputSpec {
            device: "fake".to_owned(),
            rate_hz: spec.rate_hz,
            layout: spec.layout,
            format: DeviceFormat::S32Le,
            valid_bits: 32,
        };
        let samples = [-1.0, 1.0, 0.5, -0.5, 0.25, -0.25, 2.0, -2.0];
        let block = DecodedBlock::pcm_block(spec, 4, DecodedPcm::F32(&samples));
        let mut bytes = Vec::new();
        pack_float_to_int(block, &output, &mut bytes).unwrap();
        let actual: Vec<i32> = bytes
            .chunks_exact(4)
            .map(|chunk| i32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(actual, [
            i32::MIN, i32::MAX, 1_073_741_824, -1_073_741_824,
            536_870_912, -536_870_912, i32::MAX, i32::MIN,
        ]);
        assert_eq!(quantize_float(1.0 / 65_536.0, 16).unwrap(), 0);
        assert_eq!(quantize_float(3.0 / 65_536.0, 16).unwrap(), 2);
    }

    #[test]
    fn float_compatibility_packs_valid_bits_and_rejects_nonfinite() {
        let spec = StreamSpec { encoding: SampleEncoding::F32, ..test_spec(44_100) };
        let mut output = OutputSpec {
            device: "fake".to_owned(),
            rate_hz: spec.rate_hz,
            layout: spec.layout,
            format: DeviceFormat::S24In32High,
            valid_bits: 24,
        };
        let samples = [-1.0, 0.5];
        let mut bytes = Vec::new();
        pack_float_to_int(
            DecodedBlock::pcm_block(spec, 1, DecodedPcm::F32(&samples)),
            &output,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(&bytes[..4], &i32::MIN.to_le_bytes());
        assert_eq!(&bytes[4..], &1_073_741_824_i32.to_le_bytes());
        output.format = DeviceFormat::S24_3Le;
        pack_float_to_int(
            DecodedBlock::pcm_block(spec, 1, DecodedPcm::F32(&samples)),
            &output,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, [0, 0, 0x80, 0, 0, 0x40]);
        output.format = DeviceFormat::S24In32Low;
        pack_float_to_int(
            DecodedBlock::pcm_block(spec, 1, DecodedPcm::F32(&samples)),
            &output,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, [(-8_388_608_i32).to_le_bytes(), 4_194_304_i32.to_le_bytes()].concat());
        output.format = DeviceFormat::S16Le;
        output.valid_bits = 16;
        pack_float_to_int(
            DecodedBlock::pcm_block(spec, 1, DecodedPcm::F32(&samples)),
            &output,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, [(-32_768_i16).to_le_bytes(), 16_384_i16.to_le_bytes()].concat());
        assert!(matches!(
            pack_float_to_int(
                DecodedBlock::pcm_block(spec, 1, DecodedPcm::F32(&[f32::NAN, 0.0])),
                &output,
                &mut bytes,
            ),
            Err(PlayerError::Decode)
        ));
    }

    #[test]
    fn opt_in_float_fallback_plays_integer_bytes_and_marks_event() {
        let mut float = test_spec(44_100);
        float.encoding = SampleEncoding::F32;
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("float.mp3"), FakeTrack::Pcm(float, 512));
        let harness = spawn_engine(plan);
        harness.commands.send(PlayerCommand::SetFloatToInt(true)).unwrap();
        enqueue(&harness, "float.mp3");
        harness.commands.send(PlayerCommand::Play).unwrap();
        let event = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { .. })
        });
        assert!(matches!(event, PlayerEvent::Playing { converted: true, output: OutputSpec { format: DeviceFormat::S32Le, .. }, .. }));
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::EndOfQueue));
        let drained = harness.drained.lock();
        assert_eq!(drained.as_slice(), [536_870_912_i32.to_le_bytes(); 1024].concat());
        drop(drained);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn converted_seek_reconfigures_and_remains_converted() {
        let mut float = test_spec(44_100);
        float.encoding = SampleEncoding::F32;
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("float.mp3"), FakeTrack::Pcm(float, 8192));
        let harness = spawn_engine(plan);
        harness.commands.send(PlayerCommand::SetFloatToInt(true)).unwrap();
        enqueue(&harness, "float.mp3");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { converted: true, .. })
        });
        harness.commands.send(PlayerCommand::SeekFrame(1025)).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Position { frame: 1025, .. })
        });
        assert_eq!(harness.calls.lock().starts, 2);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn disabling_compatibility_does_not_convert_next_gapless_track() {
        let mut float = test_spec(44_100);
        float.encoding = SampleEncoding::F32;
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("first.mp3"), FakeTrack::Pcm(float, 100_000));
        plan.insert(PathBuf::from("second.mp3"), FakeTrack::Pcm(float, 512));
        let harness = spawn_engine(plan);
        harness.commands.send(PlayerCommand::SetFloatToInt(true)).unwrap();
        enqueue(&harness, "first.mp3");
        enqueue(&harness, "second.mp3");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, converted: true, .. })
        });
        harness.commands.send(PlayerCommand::SetFloatToInt(false)).unwrap();
        let error = recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Error { track: Some(2), kind: PlayerError::UnsupportedFormat { .. } })
        });
        assert!(matches!(error, PlayerEvent::Error { track: Some(2), .. }));
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn adjacent_converted_tracks_stay_gapless_and_labeled() {
        let mut float = test_spec(44_100);
        float.encoding = SampleEncoding::F32;
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("first.mp3"), FakeTrack::Pcm(float, 512));
        plan.insert(PathBuf::from("second.mp3"), FakeTrack::Pcm(float, 512));
        let harness = spawn_engine(plan);
        harness.commands.send(PlayerCommand::SetFloatToInt(true)).unwrap();
        enqueue(&harness, "first.mp3");
        enqueue(&harness, "second.mp3");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 1, converted: true, .. })
        });
        recv_until(&harness.events, |event| {
            matches!(event, PlayerEvent::Playing { track: 2, converted: true, .. })
        });
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::EndOfQueue));
        assert_eq!(harness.calls.lock().starts, 1, "same spec should keep one output stream");
        assert_eq!(harness.drained.lock().as_slice(), [536_870_912_i32.to_le_bytes(); 2048].concat());
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }
    #[test]
    fn duplicate_paths_get_distinct_monotonic_ids() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "a.flac");
        let (_, pending) = recv_queue_changed(&harness.events);
        let (_, pending2) = recv_queue_changed(&harness.events);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending2.len(), 2);
        assert_eq!(pending2[0].entry.path, PathBuf::from("a.flac"));
        assert_eq!(pending2[1].entry.path, PathBuf::from("a.flac"));
        assert!(pending2[0].id < pending2[1].id, "IDs must be distinct and increasing");
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn shuffle_snapshot_is_permutation_and_disabling_preserves_order() {
        let harness = spawn_engine(HashMap::new());
        for index in 0..12 {
            enqueue(&harness, &format!("song-{index}.flac"));
        }
        let (_, original) = recv_queue_until(&harness.events, |_, pending| pending.len() == 12);
        let ids: Vec<_> = original.iter().map(|item| item.id).collect();
        harness.commands.send(PlayerCommand::SetShuffle(true)).unwrap();
        let (_, shuffled) = recv_queue_changed(&harness.events);
        let mut shuffled_ids: Vec<_> = shuffled.iter().map(|item| item.id).collect();
        shuffled_ids.sort_unstable();
        assert_eq!(shuffled_ids, ids, "shuffle retains every track exactly once");
        harness.commands.send(PlayerCommand::SetShuffle(false)).unwrap();
        let (_, unshuffled) = recv_queue_changed(&harness.events);
        assert_eq!(unshuffled, shuffled, "turning random play off must not reshuffle pending");
        enqueue(&harness, "new.flac");
        let (_, appended) = recv_queue_changed(&harness.events);
        assert_eq!(&appended[..12], &shuffled[..]);
        assert_eq!(appended[12].entry.path, PathBuf::from("new.flac"));
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn shuffle_during_play_keeps_audible_and_prepared_next() {
        let mut plan = HashMap::new();
        for name in ["a.flac", "b.flac", "c.flac", "d.flac"] {
            plan.insert(PathBuf::from(name), FakeTrack::Pcm(test_spec(44_100), 100_000));
        }
        let harness = spawn_engine(plan);
        for name in ["a.flac", "b.flac", "c.flac", "d.flac"] {
            enqueue(&harness, name);
        }
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 1, .. }));
        let (audible, pending) = recv_queue_until(&harness.events, |audible, pending| {
            audible.as_ref().is_some_and(|track| track.id == 1) && pending.len() == 3
        });
        assert_eq!(audible.unwrap().id, 1);
        assert_eq!(pending.iter().map(|item| item.id).collect::<Vec<_>>(), [2, 3, 4]);
        harness.commands.send(PlayerCommand::SetShuffle(true)).unwrap();
        let (audible, pending) = recv_queue_changed(&harness.events);
        assert_eq!(audible.unwrap().id, 1);
        assert_eq!(pending[0].id, 2, "preopened next must not be displaced");
        let mut rest = [pending[1].id, pending[2].id];
        rest.sort_unstable();
        assert_eq!(rest, [3, 4]);
        harness.commands.send(PlayerCommand::Next).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 2, .. }));
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn snapshot_shows_prepared_track_and_audible_lags_gapless_boundary() {
        // Two same-spec tracks: the second is preopened while the first
        // plays. Before the boundary, the snapshot must show the OLD track
        // as audible and the new one pending.
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 4096));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 4096));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 1, .. }));
        // Snapshots after start: audible 1; b (id 2) appears pending once
        // preopened.
        let (audible, pending) = recv_queue_until(&harness.events, |_, pending| {
            pending.iter().any(|item| item.id == 2)
        });
        assert_eq!(audible.map(|item| item.id), Some(1));
        assert_eq!(pending.iter().map(|item| item.id).collect::<Vec<_>>(), [2]);
        // After the gapless boundary the new track becomes audible.
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 2, .. }));
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn stop_returns_prepared_track_to_pending() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 100_000));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 1, .. }));
        // Wait until b is prepared (popped from the queue)...
        let (audible, pending) = recv_queue_until(&harness.events, |_, pending| {
            pending.iter().any(|item| item.id == 2)
        });
        assert_eq!(audible.map(|item| item.id), Some(1));
        assert_eq!(pending.iter().map(|item| item.id).collect::<Vec<_>>(), [2]);
        // ...then Stop must return the canceled prepared track to pending
        // instead of dropping it.
        harness.commands.send(PlayerCommand::Stop).unwrap();
        let (_, pending) = recv_queue_changed(&harness.events);
        assert_eq!(pending.iter().map(|item| item.id).collect::<Vec<_>>(), [2]);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn queue_snapshot_clears_on_end_of_queue() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::EndOfQueue));
        let (audible, pending) = recv_queue_changed(&harness.events);
        assert_eq!(audible, None);
        assert!(pending.is_empty());
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn auto_timing_follows_each_tracks_rate() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("low.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        plan.insert(PathBuf::from("high.flac"), FakeTrack::Pcm(test_spec(96_000), 512));
        let harness = spawn_engine_auto(plan);
        enqueue(&harness, "low.flac");
        enqueue(&harness, "high.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::EndOfQueue));
        let calls = harness.calls.lock();
        assert_eq!(
            calls.buffers_seen[0],
            BufferConfig { period_frames: 2_205, buffer_frames: 6_615, ring_frames: 11_025 },
            "44.1 kHz auto timing"
        );
        assert_eq!(
            calls.buffers_seen[1],
            BufferConfig { period_frames: 4_800, buffer_frames: 14_400, ring_frames: 24_000 },
            "96 kHz auto timing"
        );
        drop(calls);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn custom_timing_pair_reaches_output() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine_timed(
            plan,
            Duration::from_secs(5),
            DeviceFormat::Dop24,
            Some(1_024),
            Some(4_096),
        );
        enqueue(&harness, "a.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { .. }));
        let calls = harness.calls.lock();
        assert_eq!(calls.buffers_seen[0].period_frames, 1_024);
        assert_eq!(calls.buffers_seen[0].buffer_frames, 4_096);
        assert_eq!(calls.buffers_seen[0].ring_frames, 16_384);
        drop(calls);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }

    #[test]
    fn set_timing_command_applies_to_next_configure() {
        let mut plan = HashMap::new();
        plan.insert(PathBuf::from("a.flac"), FakeTrack::Pcm(test_spec(44_100), 100_000));
        plan.insert(PathBuf::from("b.flac"), FakeTrack::Pcm(test_spec(44_100), 512));
        let harness = spawn_engine(plan);
        enqueue(&harness, "a.flac");
        enqueue(&harness, "b.flac");
        harness.commands.send(PlayerCommand::Play).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 1, .. }));
        harness
            .commands
            .send(PlayerCommand::SetTiming {
                period_frames: Some(512),
                buffer_frames: Some(2_048),
            })
            .unwrap();
        harness.commands.send(PlayerCommand::Next).unwrap();
        recv_until(&harness.events, |event| matches!(event, PlayerEvent::Playing { track: 2, .. }));
        let calls = harness.calls.lock();
        let last = calls.buffers_seen.last().unwrap();
        assert_eq!(last.period_frames, 512);
        assert_eq!(last.buffer_frames, 2_048);
        drop(calls);
        harness.commands.send(PlayerCommand::Quit).unwrap();
        harness.join().unwrap();
    }
}
