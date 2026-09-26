use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub type TrackId = u64;
pub type DeviceId = String;

pub type CoreResult<T> = Result<T, PlayerError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    S16,
    S24,
    S32,
    F32,
    /// Raw DSD bitstream bytes; `bytes_per_sample` counts one byte = 8 DSD bits.
    Dsd,
}

impl SampleEncoding {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S32 | Self::F32 => 4,
            Self::Dsd => 1,
        }
    }

    pub const fn valid_bits(self) -> u8 {
        match self {
            Self::S16 => 16,
            Self::S24 => 24,
            Self::S32 | Self::F32 => 32,
            Self::Dsd => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelLayout {
    pub channels: u16,
    pub mask: u32,
}

impl ChannelLayout {
    pub const fn new(channels: u16, mask: u32) -> Self {
        Self { channels, mask }
    }

    pub const fn discrete(channels: u16) -> Self {
        Self { channels, mask: 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSpec {
    pub rate_hz: u32,
    pub layout: ChannelLayout,
    pub encoding: SampleEncoding,
}

impl StreamSpec {
    pub const fn bytes_per_frame(self) -> usize {
        self.encoding.bytes_per_sample() * self.layout.channels as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceFormat {
    S16Le,
    S24_3Le,
    S24In32Low,
    S24In32High,
    S32Le,
    F32Le,
    /// Native DSD, 8 bits per byte per channel.
    DsdU8,
    /// Native DSD, 16-bit little-endian slots per channel.
    DsdU16Le,
    /// Native DSD, 32-bit little-endian slots per channel.
    DsdU32Le,
    /// DoP: packed 24-bit words (marker byte in the top 8 bits) per channel.
    Dop24,
}

impl DeviceFormat {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::S16Le => 2,
            Self::S24_3Le | Self::Dop24 => 3,
            Self::S24In32Low | Self::S24In32High | Self::S32Le | Self::F32Le => 4,
            Self::DsdU8 => 1,
            Self::DsdU16Le => 2,
            Self::DsdU32Le => 4,
        }
    }

    pub const fn native_encoding(self) -> SampleEncoding {
        match self {
            Self::S16Le => SampleEncoding::S16,
            Self::S24_3Le | Self::S24In32Low | Self::S24In32High => SampleEncoding::S24,
            Self::S32Le => SampleEncoding::S32,
            Self::F32Le => SampleEncoding::F32,
            Self::DsdU8 | Self::DsdU16Le | Self::DsdU32Le | Self::Dop24 => {
                SampleEncoding::Dsd
            }
        }
    }

    pub const fn is_dsd(self) -> bool {
        matches!(
            self,
            Self::DsdU8 | Self::DsdU16Le | Self::DsdU32Le | Self::Dop24
        )
    }

    /// Decoder position/counter units represented by one output wire frame.
    /// PCM counts sample frames; DSD counts per-channel raw bytes.
    pub const fn source_frames_per_wire_frame(self) -> u64 {
        match self {
            Self::DsdU16Le | Self::Dop24 => 2,
            Self::DsdU32Le => 4,
            _ => 1,
        }
    }

    pub const fn container_bits(self) -> u8 {
        (self.bytes_per_sample() * 8) as u8
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpec {
    pub device: DeviceId,
    pub rate_hz: u32,
    pub layout: ChannelLayout,
    pub format: DeviceFormat,
    pub valid_bits: u8,
}

impl OutputSpec {
    pub fn stream_compatible(&self, input: &StreamSpec) -> bool {
        self.rate_hz == input.rate_hz
            && self.layout == input.layout
            && self.format.native_encoding() == input.encoding
            && self.valid_bits >= input.encoding.valid_bits()
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.format.bytes_per_sample() * self.layout.channels as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferConfig {
    pub period_frames: u32,
    pub buffer_frames: u32,
    pub ring_frames: u32,
}

impl BufferConfig {
    pub fn default_for_rate(rate_hz: u32) -> Self {
        Self {
            period_frames: rate_hz.div_ceil(20),
            buffer_frames: rate_hz.div_ceil(10) * 3 / 2,
            ring_frames: rate_hz.div_ceil(4),
        }
    }
}

#[derive(Debug, Default)]
pub struct OutputCounters {
    pub played_frames: AtomicU64,
    pub xruns: AtomicU64,
    pub fault: AtomicBool,
}

impl OutputCounters {
    pub fn played_frames(&self) -> u64 {
        self.played_frames.load(Ordering::Relaxed)
    }

    pub fn xruns(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    pub fn fault(&self) -> bool {
        self.fault.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerError {
    UnsupportedFormat {
        rate_hz: u32,
        channels: u16,
        encoding: SampleEncoding,
        reason: &'static str,
    },
    InvalidInput(&'static str),
    Io(std::io::ErrorKind),
    DeviceBusy,
    DeviceLost,
    Decode,
    Output,
}

impl fmt::Display for PlayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat {
                rate_hz,
                channels,
                encoding,
                reason,
            } => write!(
                f,
                "unsupported exact output format {rate_hz} Hz / {channels} channels / {encoding:?}: {reason}"
            ),
            Self::InvalidInput(reason) => f.write_str(reason),
            Self::Io(kind) => write!(f, "I/O error: {kind:?}"),
            Self::DeviceBusy => f.write_str("audio device is busy"),
            Self::DeviceLost => f.write_str("audio device was lost"),
            Self::Decode => f.write_str("decode failed"),
            Self::Output => f.write_str("output failed"),
        }
    }
}

impl std::error::Error for PlayerError {}


/// A playable queue item: one file, optionally restricted to a CUE sub-range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub path: PathBuf,
    /// CUE track bounds in CD frames (75 Hz); converted to sample frames with
    /// the decoder's actual rate at playback start. `None` plays the whole file.
    pub cue_range: Option<CueRange>,
}

impl From<PathBuf> for QueueEntry {
    fn from(path: PathBuf) -> Self {
        Self {
            path,
            cue_range: None,
        }
    }
}

/// CUE track bounds in CD frames (75 Hz); `end_cd == None` means source EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueRange {
    pub start_cd: u64,
    pub end_cd: Option<u64>,
}

/// Convert CD frames (75 Hz) to sample frames at `rate_hz` (floor). At
/// 44.1 kHz a CD frame is exactly 588 samples.
pub fn cd_frames_to_samples(cd_frames: u64, rate_hz: u32) -> u64 {
    cd_frames * u64::from(rate_hz) / 75
}

pub enum PlayerCommand {
    Enqueue(QueueEntry),
    Play,
    Pause,
    Stop,
    Next,
    SeekFrame(u64),
    SelectDevice(DeviceId),
    Quit,
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Playing {
        track: TrackId,
        output: OutputSpec,
    },
    Position {
        track: TrackId,
        frame: u64,
    },
    Paused,
    Reconfiguring,
    Underrun {
        track: TrackId,
    },
    Stalled {
        track: TrackId,
    },
    Error {
        track: Option<TrackId>,
        kind: PlayerError,
    },
    Tags {
        track: TrackId,
        tags: TrackTags,
    },
    EndOfQueue,
}

/// Read-only metadata tags for one track; every field optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

pub trait AudioOutput: Send {
    fn configure(
        &mut self,
        input: &StreamSpec,
        buffers: BufferConfig,
    ) -> Result<OutputSpec, PlayerError>;
    fn start(
        &mut self,
        pcm: rtrb::Consumer<u8>,
        counters: Arc<OutputCounters>,
    ) -> Result<(), PlayerError>;
    fn stop(&mut self) -> Result<(), PlayerError>;
    /// Configure for native DSD or DoP. Default: backend does not do DSD.
    fn configure_dsd(
        &mut self,
        spec: &DsdSpec,
        _buffers: BufferConfig,
    ) -> Result<OutputSpec, PlayerError> {
        Err(PlayerError::UnsupportedFormat {
            rate_hz: spec.dsd_rate_hz,
            channels: spec.layout.channels,
            encoding: SampleEncoding::Dsd,
            reason: "backend does not support DSD output",
        })
    }
}

pub trait Source: std::io::Read + std::io::Seek + Send + Sync {
    fn size_hint(&self) -> Option<u64>;
}

/// DSD stream parameters — a distinct type, never disguised as PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsdSpec {
    /// DSD bit rate per channel (2_822_400 = DSD64, 5_644_800 = DSD128).
    pub dsd_rate_hz: u32,
    pub layout: ChannelLayout,
}

/// What a decoder actually produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedSpec {
    Pcm(StreamSpec),
    Dsd(DsdSpec),
}

impl DecodedSpec {
    pub const fn pcm(self) -> Option<StreamSpec> {
        match self {
            Self::Pcm(spec) => Some(spec),
            Self::Dsd(_) => None,
        }
    }

    pub const fn dsd(self) -> Option<DsdSpec> {
        match self {
            Self::Dsd(spec) => Some(spec),
            Self::Pcm(_) => None,
        }
    }
}

pub enum DecodedPcm<'a> {
    I16(&'a [i16]),
    I24(&'a [i32]),
    I32(&'a [i32]),
    F32(&'a [f32]),
    /// Raw channel-interleaved DSD bytes (8 DSD bits per byte per channel).
    Dsd(&'a [u8]),
}

impl DecodedPcm<'_> {
    pub const fn encoding(&self) -> SampleEncoding {
        match self {
            Self::I16(_) => SampleEncoding::S16,
            Self::I24(_) => SampleEncoding::S24,
            Self::I32(_) => SampleEncoding::S32,
            Self::F32(_) => SampleEncoding::F32,
            Self::Dsd(_) => SampleEncoding::Dsd,
        }
    }
}

/// One decoded block. `spec` is `DecodedSpec::Pcm` for I16/I24/I32/F32 data
/// and `DecodedSpec::Dsd` for Dsd data. For PCM, `frames` counts sample
/// frames; for DSD, `frames` counts per-channel DSD bytes.
pub struct DecodedBlock<'a> {
    pub spec: DecodedSpec,
    pub frames: u32,
    pub pcm: DecodedPcm<'a>,
}

impl<'a> DecodedBlock<'a> {
    pub const fn new(spec: DecodedSpec, frames: u32, pcm: DecodedPcm<'a>) -> Self {
        Self { spec, frames, pcm }
    }

    pub const fn pcm_block(spec: StreamSpec, frames: u32, pcm: DecodedPcm<'a>) -> Self {
        Self {
            spec: DecodedSpec::Pcm(spec),
            frames,
            pcm,
        }
    }
}

pub trait Decoder: Send {
    fn spec(&self) -> StreamSpec;
    fn next_block(&mut self) -> Result<Option<DecodedBlock<'_>>, PlayerError>;
    fn seek_to_frame(&mut self, frame: u64) -> Result<u64, PlayerError>;
    /// Some(..) when this decoder outputs a DSD bitstream; then `spec()` is
    /// not meaningful and every block carries `DecodedPcm::Dsd` with
    /// `DecodedSpec::Dsd`. DSD is never disguised as PCM.
    fn dsd_spec(&self) -> Option<DsdSpec> {
        None
    }
}

pub fn pack_exact(
    input: DecodedBlock<'_>,
    output: &OutputSpec,
    dst: &mut Vec<u8>,
) -> CoreResult<()> {
    let DecodedSpec::Pcm(spec) = input.spec else {
        return Err(PlayerError::UnsupportedFormat {
            rate_hz: 0,
            channels: 0,
            encoding: SampleEncoding::Dsd,
            reason: "pack_exact handles PCM only; DSD uses DopPacker or raw passthrough",
        });
    };
    dst.clear();
    dst.resize(input.frames as usize * output.bytes_per_frame(), 0);
    let channels = spec.layout.channels as usize;
    let samples = input.frames as usize * channels;

    if !output.stream_compatible(&spec) {
        return Err(PlayerError::UnsupportedFormat {
            rate_hz: spec.rate_hz,
            channels: spec.layout.channels,
            encoding: spec.encoding,
            reason: "device format does not preserve the decoded PCM format",
        });
    }

    match (input.pcm, output.format) {
        (DecodedPcm::I16(samples_ref), DeviceFormat::S16Le) => {
            if samples_ref.len() != samples {
                return Err(PlayerError::InvalidInput(
                    "decoded I16 sample count does not match frame count",
                ));
            }
            for (sample, bytes) in samples_ref.iter().zip(dst.as_chunks_mut::<2>().0) {
                bytes.copy_from_slice(&sample.to_le_bytes());
            }
        }
        (DecodedPcm::I24(samples_ref), DeviceFormat::S24_3Le) => {
            if samples_ref.len() != samples {
                return Err(PlayerError::InvalidInput(
                    "decoded I24 sample count does not match frame count",
                ));
            }
            for (&sample, bytes) in samples_ref.iter().zip(dst.as_chunks_mut::<3>().0) {
                let le = sample.to_le_bytes();
                bytes.copy_from_slice(&le[..3]);
            }
        }
        (DecodedPcm::I24(samples_ref), DeviceFormat::S24In32Low) => {
            if samples_ref.len() != samples {
                return Err(PlayerError::InvalidInput(
                    "decoded I24 sample count does not match frame count",
                ));
            }
            for (&sample, bytes) in samples_ref.iter().zip(dst.as_chunks_mut::<4>().0) {
                bytes[..3].copy_from_slice(&sample.to_le_bytes()[..3]);
                bytes[3] = if sample < 0 { 0xff } else { 0 };
            }
        }
        (DecodedPcm::I24(samples_ref), DeviceFormat::S24In32High) => {
            if samples_ref.len() != samples {
                return Err(PlayerError::InvalidInput(
                    "decoded I24 sample count does not match frame count",
                ));
            }
            for (&sample, bytes) in samples_ref.iter().zip(dst.as_chunks_mut::<4>().0) {
                bytes[1..4].copy_from_slice(&sample.to_le_bytes()[..3]);
            }
        }
        (DecodedPcm::I32(samples_ref), DeviceFormat::S32Le) => {
            if samples_ref.len() != samples {
                return Err(PlayerError::InvalidInput(
                    "decoded I32 sample count does not match frame count",
                ));
            }
            for (sample, bytes) in samples_ref.iter().zip(dst.as_chunks_mut::<4>().0) {
                bytes.copy_from_slice(&sample.to_le_bytes());
            }
        }
        (DecodedPcm::F32(samples_ref), DeviceFormat::F32Le) => {
            if samples_ref.len() != samples {
                return Err(PlayerError::InvalidInput(
                    "decoded F32 sample count does not match frame count",
                ));
            }
            for (sample, bytes) in samples_ref.iter().zip(dst.as_chunks_mut::<4>().0) {
                bytes.copy_from_slice(&sample.to_le_bytes());
            }
        }
        (pcm, format) => {
            return Err(PlayerError::UnsupportedFormat {
                rate_hz: spec.rate_hz,
                channels: spec.layout.channels,
                encoding: pcm.encoding(),
                reason: match (pcm.encoding(), format) {
                    (
                        SampleEncoding::S24,
                        DeviceFormat::S24_3Le
                        | DeviceFormat::S24In32Low
                        | DeviceFormat::S24In32High,
                    )
                    | (SampleEncoding::S16, DeviceFormat::S16Le)
                    | (SampleEncoding::S32, DeviceFormat::S32Le)
                    | (SampleEncoding::F32, DeviceFormat::F32Le) => {
                        unreachable!("compatible format returned an error")
                    }
                    _ => "sample format conversion is forbidden in bit-perfect mode",
                },
            });
        }
    }
    Ok(())
}

pub fn validate_exact_spec(input: &StreamSpec) -> CoreResult<()> {
    if input.rate_hz == 0 {
        return Err(PlayerError::InvalidInput(
            "stream sample rate must be nonzero",
        ));
    }
    if input.layout.channels == 0 {
        return Err(PlayerError::InvalidInput(
            "stream must contain at least one channel",
        ));
    }
    Ok(())
}

pub fn validate_device_selection(device: &str) -> CoreResult<()> {
    if device.starts_with("hw:") {
        Ok(())
    } else {
        Err(PlayerError::InvalidInput(
            "M1 requires an explicit direct ALSA device in the form hw:CARD,DEV",
        ))
    }
}

/// Group channel-interleaved DSD bytes into native ALSA slots without
/// changing any bit. Input bytes are ordered by time then channel; output
/// frames are ordered by channel, with `slot_bytes` consecutive time bytes
/// in each little-endian U8/U16/U32 slot.
pub fn pack_native_dsd(
    input: &[u8],
    channels: u16,
    slot_bytes: usize,
    out: &mut Vec<u8>,
) -> CoreResult<()> {
    let channels = usize::from(channels);
    if channels == 0 || !matches!(slot_bytes, 1 | 2 | 4) {
        return Err(PlayerError::InvalidInput("invalid native DSD slot layout"));
    }
    let frame_bytes = channels * slot_bytes;
    if input.len() % frame_bytes != 0 {
        return Err(PlayerError::InvalidInput(
            "DSD block must contain whole native DSD slots per channel",
        ));
    }
    out.clear();
    out.reserve(input.len());
    for frame in 0..input.len() / frame_bytes {
        for channel in 0..channels {
            for byte in 0..slot_bytes {
                out.push(input[(frame * slot_bytes + byte) * channels + channel]);
            }
        }
    }
    Ok(())
}

/// DoP (DSD over PCM) framing: converts raw channel-interleaved DSD bytes
/// into packed 24-bit little-endian words. The marker byte (0x05/0xFA) is
/// shared by all channels and alternates **each PCM frame**; the other two
/// bytes carry 16 unchanged DSD bits. Matches Linux USB DoP's L1 L2 marker,
/// R1 R2 marker layout. Phase persists across blocks and seamless tracks.
pub struct DopPacker {
    channels: usize,
    marker: u8,
}

impl DopPacker {
    pub fn new(channels: u16) -> Self {
        Self {
            channels: usize::from(channels),
            marker: 0x05,
        }
    }

    pub fn pack(&mut self, dsd_interleaved: &[u8], out: &mut Vec<u8>) -> CoreResult<()> {
        if self.channels == 0 {
            return Err(PlayerError::InvalidInput("DoP packer needs channels"));
        }
        if dsd_interleaved.len() % (self.channels * 2) != 0 {
            return Err(PlayerError::InvalidInput(
                "DoP packing requires an even per-channel DSD byte count",
            ));
        }
        let words = dsd_interleaved.len() / (2 * self.channels);
        out.clear();
        out.reserve_exact(words * self.channels * 3);
        for word in 0..words {
            for channel in 0..self.channels {
                let low = dsd_interleaved[(2 * word) * self.channels + channel];
                let high = dsd_interleaved[(2 * word + 1) * self.channels + channel];
                out.extend_from_slice(&[low, high, self.marker]);
            }
            self.marker = if self.marker == 0x05 { 0xfa } else { 0x05 };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> ChannelLayout {
        ChannelLayout::discrete(2)
    }

    fn spec(encoding: SampleEncoding) -> StreamSpec {
        StreamSpec {
            rate_hz: 48_000,
            layout: layout(),
            encoding,
        }
    }

    #[test]
    fn rejects_hidden_float_to_integer_conversion() {
        let output = OutputSpec {
            device: "hw:1,0".to_owned(),
            rate_hz: 48_000,
            layout: layout(),
            format: DeviceFormat::S16Le,
            valid_bits: 16,
        };
        let err = pack_exact(
            DecodedBlock::pcm_block(spec(SampleEncoding::F32), 1, DecodedPcm::F32(&[0.0, -0.0])),
            &output,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlayerError::UnsupportedFormat {
                encoding: SampleEncoding::F32,
                ..
            }
        ));
    }

    #[test]
    fn rejects_24_bit_narrowing_to_16_bit() {
        let output = OutputSpec {
            device: "hw:1,0".to_owned(),
            rate_hz: 48_000,
            layout: layout(),
            format: DeviceFormat::S16Le,
            valid_bits: 16,
        };
        let err = pack_exact(
            DecodedBlock::pcm_block(spec(SampleEncoding::S24), 1, DecodedPcm::I24(&[0, 1])),
            &output,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PlayerError::UnsupportedFormat {
                encoding: SampleEncoding::S24,
                ..
            }
        ));
    }

    #[test]
    fn packs_s24_three_bytes_little_endian() {
        let output = OutputSpec {
            device: "hw:1,0".to_owned(),
            rate_hz: 48_000,
            layout: layout(),
            format: DeviceFormat::S24_3Le,
            valid_bits: 24,
        };
        let mut bytes = Vec::new();
        pack_exact(
            DecodedBlock::pcm_block(
                spec(SampleEncoding::S24),
                1,
                DecodedPcm::I24(&[0x123456, -2]),
            ),
            &output,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, [0x56, 0x34, 0x12, 0xfe, 0xff, 0xff]);
    }

    #[test]
    fn packs_s24_into_low_24_bits_of_s32() {
        let output = OutputSpec {
            device: "hw:1,0".to_owned(),
            rate_hz: 48_000,
            layout: layout(),
            format: DeviceFormat::S24In32Low,
            valid_bits: 24,
        };
        let mut bytes = Vec::new();
        pack_exact(
            DecodedBlock::pcm_block(
                spec(SampleEncoding::S24),
                1,
                DecodedPcm::I24(&[0x123456, -2]),
            ),
            &output,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, [0x56, 0x34, 0x12, 0x00, 0xfe, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn requires_direct_hw_device_name() {
        assert!(validate_device_selection("hw:1,0").is_ok());
        assert!(matches!(
            validate_device_selection("plughw:1,0"),
            Err(PlayerError::InvalidInput(_))
        ));
    }

    #[test]
    fn native_dsd_slots_preserve_bits_and_group_by_channel() {
        // Time/channel order: L0 R0 L1 R1 L2 R2 L3 R3.
        let raw = [0x12, 0xa1, 0x34, 0xb2, 0x56, 0xc3, 0x78, 0xd4];
        let mut out = Vec::new();
        pack_native_dsd(&raw, 2, 4, &mut out).unwrap();
        assert_eq!(out, [0x12, 0x34, 0x56, 0x78, 0xa1, 0xb2, 0xc3, 0xd4]);
        pack_native_dsd(&raw, 2, 2, &mut out).unwrap();
        assert_eq!(out, [0x12, 0x34, 0xa1, 0xb2, 0x56, 0x78, 0xc3, 0xd4]);
        pack_native_dsd(&raw, 2, 1, &mut out).unwrap();
        assert_eq!(out, raw);
        assert!(matches!(
            pack_native_dsd(&raw[..6], 2, 4, &mut out),
            Err(PlayerError::InvalidInput(_))
        ));
    }

    #[test]
    fn dsd_wire_frame_units_are_explicit() {
        assert_eq!(DeviceFormat::DsdU8.source_frames_per_wire_frame(), 1);
        assert_eq!(DeviceFormat::DsdU16Le.source_frames_per_wire_frame(), 2);
        assert_eq!(DeviceFormat::DsdU32Le.source_frames_per_wire_frame(), 4);
        assert_eq!(DeviceFormat::Dop24.source_frames_per_wire_frame(), 2);
        assert_eq!(DeviceFormat::S16Le.source_frames_per_wire_frame(), 1);
    }

    #[test]
    fn dop_frames_marker_and_bit_order() {
        let mut packer = DopPacker::new(2);
        // Two stereo PCM frames: each channel shares the marker, then it
        // alternates for the next frame.
        let mut out = Vec::new();
        packer.pack(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88], &mut out).unwrap();
        assert_eq!(out, [
            0x11, 0x33, 0x05, 0x22, 0x44, 0x05,
            0x55, 0x77, 0xfa, 0x66, 0x88, 0xfa,
        ]);
    }

    #[test]
    fn dop_marker_alternates_each_frame_across_blocks() {
        let mut packer = DopPacker::new(1);
        let mut out = Vec::new();
        packer.pack(&[0xaa, 0xbb], &mut out).unwrap();
        assert_eq!(out, [0xaa, 0xbb, 0x05]);
        packer.pack(&[0xcc, 0xdd], &mut out).unwrap();
        assert_eq!(out, [0xcc, 0xdd, 0xfa]);
        packer.pack(&[0xee, 0xff], &mut out).unwrap();
        assert_eq!(out, [0xee, 0xff, 0x05]);
    }

    #[test]
    fn dop_rejects_odd_per_channel_byte_count() {
        let mut packer = DopPacker::new(2);
        let mut out = Vec::new();
        assert!(matches!(
            packer.pack(&[0x01, 0x02, 0x03], &mut out),
            Err(PlayerError::InvalidInput(_))
        ));
    }

    #[test]
    fn pack_exact_rejects_dsd_blocks() {
        let output = OutputSpec {
            device: "hw:1,0".to_owned(),
            rate_hz: 176_400,
            layout: layout(),
            format: DeviceFormat::Dop24,
            valid_bits: 24,
        };
        let dsd = [0x69u8; 16];
        let block = DecodedBlock::new(
            DecodedSpec::Dsd(DsdSpec {
                dsd_rate_hz: 2_822_400,
                layout: layout(),
            }),
            8,
            DecodedPcm::Dsd(&dsd),
        );
        let mut bytes = Vec::new();
        assert!(matches!(
            pack_exact(block, &output, &mut bytes),
            Err(PlayerError::UnsupportedFormat { .. })
        ));
    }
}
