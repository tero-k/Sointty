use sointty_core::{
    ChannelLayout, DecodedBlock, DecodedPcm, Decoder, PlayerError, SampleEncoding, Source,
    StreamSpec, validate_exact_spec,
};
use symphonia::core::audio::{Audio, Channels};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;

struct CoreSource {
    source: Box<dyn Source>,
    size: Option<u64>,
}

impl std::io::Read for CoreSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.source.read(buf)
    }
}

impl std::io::Seek for CoreSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.source.seek(pos)
    }
}

impl MediaSource for CoreSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.size.or_else(|| self.source.size_hint())
    }
}

/// Which scratch buffer holds the current block; determines the encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcmKind {
    I16,
    I24,
    I32,
    F32,
}

impl PcmKind {
    fn encoding(self) -> SampleEncoding {
        match self {
            Self::I16 => SampleEncoding::S16,
            Self::I24 => SampleEncoding::S24,
            Self::I32 => SampleEncoding::S32,
            Self::F32 => SampleEncoding::F32,
        }
    }
}

pub struct SymphoniaDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    spec: StreamSpec,
    /// Bits per sample declared by the codec (amended from the FLAC
    /// STREAMINFO block). Distinguishes a true S32 stream from sub-32-bit
    /// samples the decoder widened into left-aligned S32 buffers.
    bits_per_sample: Option<u32>,
    total_frames: Option<u64>,
    position: u64,
    block_i16: Vec<i16>,
    block_i24: Vec<i32>,
    block_i32: Vec<i32>,
    block_f32: Vec<f32>,
    block_kind: PcmKind,
    block_frames: u32,
    /// The first block was decoded during `open` to learn the encoding and
    /// is returned by the first `next_block` call.
    primed: bool,
    /// False only while probing the first block of an undeclared stream.
    encoding_known: bool,
}

impl SymphoniaDecoder {
    pub fn open(source: Box<dyn Source>, hint: Option<&str>) -> Result<Self, PlayerError> {
        let size = source.size_hint();
        let stream = MediaSourceStream::new(
            Box::new(CoreSource { source, size }),
            MediaSourceStreamOptions::default(),
        );

        let mut format_hint = Hint::new();
        if let Some(extension) = hint {
            format_hint.with_extension(extension);
        }

        let format = symphonia::default::get_probe()
            .probe(
                &format_hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|_| PlayerError::Decode)?;
        let (track_id, codec_params, total_frames) = {
            let track = format
                .default_track(TrackType::Audio)
                .or_else(|| format.first_track_known_codec(TrackType::Audio))
                .ok_or(PlayerError::Decode)?;
            let codec_params: AudioCodecParameters = track
                .codec_params
                .clone()
                .and_then(|params| params.audio().cloned())
                .ok_or(PlayerError::Decode)?;
            (track.id, codec_params, track.num_frames)
        };
        let decoder = (symphonia::default::get_codecs()
            .get_audio_decoder(codec_params.codec)
            .ok_or(PlayerError::Decode)?
            .factory)(
            &codec_params,
            &AudioDecoderOptions::default().gapless(true).verify(false),
        )
        .map_err(|_| PlayerError::Decode)?;
        // The FLAC codec amends its params with the STREAMINFO bit depth at
        // construction, so this is exact even when the demuxer (Ogg) did not
        // declare it.
        let bits_per_sample = decoder.codec_params().bits_per_sample;

        let mut result = Self {
            format,
            decoder,
            track_id,
            spec: spec_from_params(&codec_params)?,
            bits_per_sample,
            position: 0,
            block_i16: Vec::new(),
            block_i24: Vec::new(),
            block_i32: Vec::new(),
            block_f32: Vec::new(),
            block_kind: PcmKind::I16,
            block_frames: 0,
            primed: false,
            total_frames,
            encoding_known: false,
        };
        // Establish the EXACT sample encoding before the engine negotiates
        // the output: from the container's declared sample format when
        // present, otherwise by decoding the first block (the same priming
        // the FFmpeg decoder does). Claiming a fixed encoding here mis-
        // configures the device and every block is then rejected at packing.
        match codec_params.sample_format {
            Some(format) => {
                let declared = encoding_of_sample_format(format)
                    .ok_or(PlayerError::Decode)?;
                result.spec.encoding = refine_encoding(declared, bits_per_sample);
            }
            None => {
                if !result.decode_next()? {
                    return Err(PlayerError::Decode);
                }
                result.spec.encoding = result.block_kind.encoding();
                result.primed = true;
            }
        }
        result.encoding_known = true;
        validate_exact_spec(&result.spec)?;
        Ok(result)
    }
}

impl Decoder for SymphoniaDecoder {
    fn spec(&self) -> StreamSpec {
        self.spec
    }
    fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    fn next_block(&mut self) -> Result<Option<DecodedBlock<'_>>, PlayerError> {
        if !self.primed && !self.decode_next()? {
            return Ok(None);
        }
        self.primed = false;
        let spec = self.spec;
        let frames = self.block_frames;
        Ok(Some(match self.block_kind {
            PcmKind::I16 => {
                DecodedBlock::pcm_block(spec, frames, DecodedPcm::I16(&self.block_i16))
            }
            PcmKind::I24 => {
                DecodedBlock::pcm_block(spec, frames, DecodedPcm::I24(&self.block_i24))
            }
            PcmKind::I32 => {
                DecodedBlock::pcm_block(spec, frames, DecodedPcm::I32(&self.block_i32))
            }
            PcmKind::F32 => {
                DecodedBlock::pcm_block(spec, frames, DecodedPcm::F32(&self.block_f32))
            }
        }))
    }
    fn seek_to_frame(&mut self, frame: u64) -> Result<u64, PlayerError> {
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    track_id: self.track_id,
                    ts: Timestamp::new(frame as i64),
                },
            )
            .map_err(|_| PlayerError::Decode)?;
        self.decoder.reset();
        self.primed = false;
        self.position = seeked.actual_ts.get() as u64;
        Ok(self.position)
    }
}

impl SymphoniaDecoder {
    /// Decode one packet into the scratch buffers, recording its kind and
    /// frame count. Returns `false` at end of stream.
    fn decode_next(&mut self) -> Result<bool, PlayerError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(false),
                Err(_) => return Err(PlayerError::Decode),
            };
            if packet.track_id != self.track_id {
                continue;
            }

            let decoded = self
                .decoder
                .decode(&packet)
                .map_err(|_| PlayerError::Decode)?;
            let frames = decoded.frames();
            if decoded.spec().rate() != self.spec.rate_hz {
                return Err(PlayerError::Decode);
            }
            self.position += frames as u64;

            self.block_kind = match decoded {
                symphonia::core::audio::GenericAudioBufferRef::S16(buffer) => {
                    self.block_i16.clear();
                    self.block_i16.reserve(buffer.samples_interleaved());
                    buffer.copy_to_vec_interleaved(&mut self.block_i16);
                    PcmKind::I16
                }
                symphonia::core::audio::GenericAudioBufferRef::S24(buffer) => {
                    self.block_i24.clear();
                    self.block_i24.reserve(buffer.samples_interleaved());
                    for sample in buffer.iter_interleaved() {
                        self.block_i24.push(sample.inner());
                    }
                    PcmKind::I24
                }
                symphonia::core::audio::GenericAudioBufferRef::S32(buffer) => {
                    // FLAC (and ALAC) decode every sub-32-bit stream into S32
                    // buffers with samples shifted left by `32 - bps`. When
                    // the codec declares 16 or 24 bits per sample, undo that
                    // widening so the output negotiates the exact source
                    // encoding (S16/S24); a device that only offers S16 or
                    // 24-in-32 physical formats then matches, where a raw
                    // S32 stream would be rejected. The right shift restores
                    // the exact source values; any sample whose widened low
                    // bits are nonzero contradicts the declared depth and is
                    // a decode error, never a silent truncation.
                    match refine_encoding(SampleEncoding::S32, self.bits_per_sample) {
                        SampleEncoding::S16 => {
                            self.block_i16.clear();
                            self.block_i16.reserve(buffer.samples_interleaved());
                            for sample in buffer.iter_interleaved() {
                                let recovered = sample >> 16;
                                if recovered << 16 != sample {
                                    return Err(PlayerError::Decode);
                                }
                                self.block_i16.push(recovered as i16);
                            }
                            PcmKind::I16
                        }
                        SampleEncoding::S24 => {
                            self.block_i24.clear();
                            self.block_i24.reserve(buffer.samples_interleaved());
                            for sample in buffer.iter_interleaved() {
                                let recovered = sample >> 8;
                                if recovered << 8 != sample {
                                    return Err(PlayerError::Decode);
                                }
                                self.block_i24.push(recovered);
                            }
                            PcmKind::I24
                        }
                        _ => {
                            self.block_i32.clear();
                            self.block_i32.reserve(buffer.samples_interleaved());
                            buffer.copy_to_vec_interleaved(&mut self.block_i32);
                            PcmKind::I32
                        }
                    }
                }
                symphonia::core::audio::GenericAudioBufferRef::F32(buffer) => {
                    self.block_f32.clear();
                    self.block_f32.reserve(buffer.samples_interleaved());
                    buffer.copy_to_vec_interleaved(&mut self.block_f32);
                    PcmKind::F32
                }
                _ => return Err(PlayerError::Decode),
            };
            // Once established, the encoding must not change mid-stream.
            if self.encoding_known && self.block_kind.encoding() != self.spec.encoding {
                return Err(PlayerError::Decode);
            }
            self.block_frames = frames as u32;
            return Ok(true);
        }
    }
}

/// Rate and layout from codec parameters. The `encoding` is a placeholder:
/// `open` always overwrites it from the declared sample format or the first
/// decoded block before the spec reaches the output negotiation.
fn spec_from_params(params: &AudioCodecParameters) -> Result<StreamSpec, PlayerError> {
    let rate = params.sample_rate.ok_or(PlayerError::Decode)?;
    let channels = params.channels.as_ref().ok_or(PlayerError::Decode)?;
    let count = u16::try_from(channels.count()).map_err(|_| PlayerError::Decode)?;
    let mask = match channels {
        Channels::Positioned(positions) => {
            u32::try_from(positions.bits()).map_err(|_| PlayerError::Decode)?
        }
        Channels::Discrete(_) => 0,
        _ => return Err(PlayerError::Decode),
    };
    Ok(StreamSpec {
        rate_hz: rate,
        layout: ChannelLayout::new(count, mask),
        encoding: SampleEncoding::S16,
    })
}

/// Map a container-declared sample format to an exact-path encoding.
/// `None`: unsigned, 8-bit or f64 samples have no exact output path here.
fn encoding_of_sample_format(
    format: symphonia::core::audio::sample::SampleFormat,
) -> Option<SampleEncoding> {
    use symphonia::core::audio::sample::SampleFormat;
    match format {
        SampleFormat::S16 => Some(SampleEncoding::S16),
        SampleFormat::S24 => Some(SampleEncoding::S24),
        SampleFormat::S32 => Some(SampleEncoding::S32),
        SampleFormat::F32 => Some(SampleEncoding::F32),
        _ => None,
    }
}

/// Recover the exact source encoding when a codec widens sub-32-bit samples
/// into left-aligned S32 buffers (Symphonia's FLAC and ALAC decoders do).
/// The declared bits per sample identify the true encoding; anything else
/// (unknown depth, 20-bit, true 32-bit) stays S32.
fn refine_encoding(encoding: SampleEncoding, bits_per_sample: Option<u32>) -> SampleEncoding {
    match (encoding, bits_per_sample) {
        (SampleEncoding::S32, Some(16)) => SampleEncoding::S16,
        (SampleEncoding::S32, Some(24)) => SampleEncoding::S24,
        _ => encoding,
    }
}

#[cfg(feature = "ffmpeg-lgpl")]
mod ffmpeg;
#[cfg(feature = "ffmpeg-lgpl")]
pub use ffmpeg::{FormatAvailability, availability as ffmpeg_availability};

#[cfg(feature = "opus")]
mod opus;
pub mod tags;

pub fn extension_for_path(path: &std::path::Path) -> Option<&str> {
    path.extension().and_then(|extension| extension.to_str())
}

/// Open Ogg Opus via libopus, optional extended formats via FFmpeg, and
/// everything else via Symphonia. Neither optional dependency is in the
/// default build.
pub fn open_decoder(
    path: &std::path::Path,
    source: Box<dyn Source>,
) -> Result<Box<dyn Decoder>, PlayerError> {
    let extension = extension_for_path(path);
    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("opus")) {
        #[cfg(feature = "opus")]
        return Ok(Box::new(opus::OggOpusDecoder::open(source)?));
        #[cfg(not(feature = "opus"))]
        return Err(PlayerError::InvalidInput(
            "Ogg Opus support not compiled in (enable the `opus` feature)",
        ));
    }
    if extension.is_some_and(|ext| {
        ["ape", "wv", "tak", "mpc", "dsf", "dff"]
            .iter()
            .any(|name| ext.eq_ignore_ascii_case(name))
    }) {
        #[cfg(feature = "ffmpeg-lgpl")]
        return Ok(Box::new(ffmpeg::FfmpegDecoder::open(
            source,
            &extension.unwrap().to_ascii_lowercase(),
        )?));
        #[cfg(not(feature = "ffmpeg-lgpl"))]
        return Err(PlayerError::InvalidInput(
            "format requires the ffmpeg-lgpl feature",
        ));
    }
    Ok(Box::new(SymphoniaDecoder::open(source, extension)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemorySource {
        data: std::io::Cursor<Vec<u8>>,
    }

    impl std::io::Read for MemorySource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.data.read(buf)
        }
    }

    impl std::io::Seek for MemorySource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.data.seek(pos)
        }
    }

    impl Source for MemorySource {
        fn size_hint(&self) -> Option<u64> {
            Some(self.data.get_ref().len() as u64)
        }
    }

    fn wav_fixture() -> Vec<u8> {
        let samples: [i16; 8] = [0, 1, -1, i16::MAX, i16::MIN, 2, -2, 3];
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&(44_100_u32 * 2 * 2).to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn streams_exact_wav_i16_samples() {
        let source = MemorySource {
            data: std::io::Cursor::new(wav_fixture()),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("wav")).unwrap();
        assert_eq!(decoder.total_frames(), Some(4));
        let block = decoder.next_block().unwrap().unwrap();
        let block_spec = block.spec.pcm().expect("PCM block");
        assert_eq!(block_spec.rate_hz, 44_100);
        assert_eq!(block_spec.layout.channels, 2);
        assert_eq!(block.frames, 4);
        match block.pcm {
            DecodedPcm::I16(samples) => {
                assert_eq!(samples, [0, 1, -1, i16::MAX, i16::MIN, 2, -2, 3]);
            }
            other => panic!("expected signed 16-bit PCM, got {:?}", other.encoding()),
        }
    }

    #[test]
    fn returns_none_at_end_after_streaming_blocks() {
        let source = MemorySource {
            data: std::io::Cursor::new(wav_fixture()),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("wav")).unwrap();
        assert!(decoder.next_block().unwrap().is_some());
        assert!(decoder.next_block().unwrap().is_none());
    }

    fn wav_header(tag: u16, bits: u16, block_align: u16, data_len: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&(44_100_u32 * u32::from(block_align)).to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes
    }

    fn f32_wav_fixture() -> Vec<u8> {
        let samples: [f32; 4] = [0.0, 0.5, -0.5, 0.25];
        let mut bytes = wav_header(3, 32, 8, (samples.len() * 4) as u32);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn s24_wav_fixture() -> Vec<u8> {
        let samples: [i32; 4] = [0, 1, -1, 0x007F_FFFF];
        let mut bytes = wav_header(1, 24, 6, (samples.len() * 3) as u32);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes()[..3]);
        }
        bytes
    }

    #[test]
    fn f32_wav_reports_f32_and_streams_f32() {
        // Regression: the spec used to hardcode S16, so the device was
        // negotiated S16 and every f32 block was rejected at packing.
        let source = MemorySource {
            data: std::io::Cursor::new(f32_wav_fixture()),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("wav")).unwrap();
        assert_eq!(decoder.spec().encoding, SampleEncoding::F32);
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.spec.pcm().unwrap().encoding, SampleEncoding::F32);
        match block.pcm {
            DecodedPcm::F32(samples) => assert_eq!(samples, [0.0, 0.5, -0.5, 0.25]),
            other => panic!("expected f32 PCM, got {:?}", other.encoding()),
        }
    }

    #[test]
    fn s24_wav_reports_s24_and_streams_s24() {
        let source = MemorySource {
            data: std::io::Cursor::new(s24_wav_fixture()),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("wav")).unwrap();
        assert_eq!(decoder.spec().encoding, SampleEncoding::S24);
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.spec.pcm().unwrap().encoding, SampleEncoding::S24);
        match block.pcm {
            DecodedPcm::I24(samples) => assert_eq!(samples, [0, 1, -1, 0x007F_FFFF]),
            other => panic!("expected s24 PCM, got {:?}", other.encoding()),
        }
    }

    #[test]
    fn mp3_priming_reports_actual_f32_before_output_negotiation() {
        // The MP3 container does not declare the decoded sample format. A
        // generated sine fixture catches the old hardcoded-S16 spec: the
        // engine must see F32 before opening the device.
        let source = MemorySource {
            data: std::io::Cursor::new(include_bytes!("fixtures/tone.mp3").to_vec()),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("mp3")).unwrap();
        assert_eq!(decoder.spec().encoding, SampleEncoding::F32);
        let block = decoder.next_block().unwrap().unwrap();
        assert!(matches!(block.pcm, DecodedPcm::F32(samples) if samples.iter().any(|&x| x != 0.0)));
        assert_eq!(block.spec.pcm().unwrap().encoding, SampleEncoding::F32);
    }

    /// FLAC CRC-8 (poly 0x07, init 0, MSB-first).
    fn crc8(data: &[u8]) -> u8 {
        let mut crc = 0u8;
        for &byte in data {
            crc ^= byte;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
            }
        }
        crc
    }

    /// FLAC CRC-16 (poly 0x8005, init 0, MSB-first).
    fn crc16(data: &[u8]) -> u16 {
        let mut crc = 0u16;
        for &byte in data {
            crc ^= u16::from(byte) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
            }
        }
        crc
    }

    /// MSB-first bit writer for FLAC frame payloads.
    struct BitWriter {
        bytes: Vec<u8>,
        acc: u64,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bytes: Vec::new(), acc: 0, nbits: 0 }
        }

        fn write(&mut self, value: u64, bits: u32) {
            assert!(bits <= 56);
            self.acc = (self.acc << bits) | (value & (u64::MAX >> (64 - bits.max(1))));
            self.nbits += bits;
            while self.nbits >= 8 {
                self.nbits -= 8;
                self.bytes.push((self.acc >> self.nbits) as u8);
            }
        }

        /// Flush with zero padding to byte alignment.
        fn finish(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                self.bytes.push((self.acc << (8 - self.nbits)) as u8);
            }
            self.bytes
        }
    }

    /// Minimal FLAC file: STREAMINFO plus one fixed-blocksize frame of
    /// verbatim subframes. `channels` holds the exact source samples,
    /// right-aligned, `bps` bits each.
    fn flac_fixture(bps: u32, channels: &[Vec<i32>]) -> Vec<u8> {
        let block = channels[0].len() as u32;
        assert!(channels.iter().all(|c| c.len() as u32 == block) && block >= 16);
        let channel_count = channels.len() as u32;
        let rate = 44_100u64;

        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        out.push(0x80); // last metadata block, type 0 = STREAMINFO
        out.extend_from_slice(&34u32.to_be_bytes()[1..]);
        out.extend_from_slice(&(block as u16).to_be_bytes()); // min block size
        out.extend_from_slice(&(block as u16).to_be_bytes()); // max block size
        out.extend_from_slice(&[0; 3]); // min frame size unknown
        out.extend_from_slice(&[0; 3]); // max frame size unknown
        let mut packed = BitWriter::new();
        packed.write(rate, 20);
        packed.write(u64::from(channel_count - 1), 3);
        packed.write(u64::from(bps - 1), 5);
        packed.write(u64::from(block), 36); // total samples
        out.extend_from_slice(&packed.finish());
        out.extend_from_slice(&[0; 16]); // MD5 unset

        let bps_code = match bps {
            16 => 0b100,
            24 => 0b110,
            32 => 0b111,
            _ => unreachable!(),
        };
        let mut frame = vec![
            0xFF,
            0xF8, // sync, reserved 0, fixed blocking strategy
            0b0111_0000, // 16-bit blocksize-1 follows; rate from STREAMINFO
            (((channel_count - 1) << 4) | (bps_code << 1)) as u8,
            0x00, // coded frame number 0
        ];
        frame.extend_from_slice(&((block - 1) as u16).to_be_bytes());
        let header_crc = crc8(&frame);
        frame.push(header_crc);
        let mut payload = BitWriter::new();
        for channel in channels {
            payload.write(0b0000001_0, 8); // verbatim subframe, no wasted bits
            for &sample in channel {
                payload.write(sample as i64 as u64, bps);
            }
        }
        frame.append(&mut payload.finish());
        let frame_crc = crc16(&frame);
        frame.extend_from_slice(&frame_crc.to_be_bytes());
        out.extend_from_slice(&frame);
        out
    }

    fn flac_samples_16() -> [Vec<i32>; 2] {
        let left: Vec<i32> = vec![
            0, 1, -1, i16::MAX as i32, i16::MIN as i32, 12_345, -12_345, 100,
            -100, 256, -256, 0, 0, 1, -1, 0,
        ];
        let right: Vec<i32> = left.iter().map(|s| -s).collect();
        [left, right]
    }

    #[test]
    fn flac_16bit_reports_s16_and_streams_exact_samples() {
        let expected = flac_samples_16();
        let source = MemorySource {
            data: std::io::Cursor::new(flac_fixture(16, &expected)),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("flac")).unwrap();
        assert_eq!(decoder.spec().encoding, SampleEncoding::S16);
        assert_eq!(decoder.total_frames(), Some(16));
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.spec.pcm().unwrap().encoding, SampleEncoding::S16);
        assert_eq!(block.frames, 16);
        match block.pcm {
            DecodedPcm::I16(samples) => {
                let want: Vec<i16> = expected[0]
                    .iter()
                    .zip(&expected[1])
                    .flat_map(|(&l, &r)| [l as i16, r as i16])
                    .collect();
                assert_eq!(samples, want.as_slice());
            }
            other => panic!("expected s16 PCM, got {:?}", other.encoding()),
        }
        assert!(decoder.next_block().unwrap().is_none());
    }

    #[test]
    fn flac_24bit_reports_s24_and_streams_exact_samples() {
        // Regression: Symphonia's FLAC decoder widens 24-bit samples into
        // left-aligned S32 buffers, so the spec claimed S32 and macOS
        // devices offering only 24-in-32 physical formats were rejected.
        let left: Vec<i32> = vec![
            0, 1, -1, 0x007F_FFFF, -0x0080_0000, 0x0012_3456, -0x0012_3456, 256,
            -256, 65_536, -65_536, 0, 0, 1, -1, 0,
        ];
        let right: Vec<i32> = left.iter().map(|s| (-s).clamp(-0x0080_0000, 0x007F_FFFF)).collect();
        let expected = [left, right];
        let source = MemorySource {
            data: std::io::Cursor::new(flac_fixture(24, &expected)),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("flac")).unwrap();
        assert_eq!(decoder.spec().encoding, SampleEncoding::S24);
        assert_eq!(decoder.total_frames(), Some(16));
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.spec.pcm().unwrap().encoding, SampleEncoding::S24);
        assert_eq!(block.frames, 16);
        match block.pcm {
            DecodedPcm::I24(samples) => {
                let want: Vec<i32> = expected[0]
                    .iter()
                    .zip(&expected[1])
                    .flat_map(|(&l, &r)| [l, r])
                    .collect();
                assert_eq!(samples, want.as_slice());
            }
            other => panic!("expected s24 PCM, got {:?}", other.encoding()),
        }
        assert!(decoder.next_block().unwrap().is_none());
    }

    #[test]
    fn flac_32bit_stays_s32_full_range() {
        // A true 32-bit stream must not be narrowed: every bit is valid.
        let left: Vec<i32> = vec![
            0, 1, -1, i32::MAX, i32::MIN, 0x1234_5678, -0x1234_5678, 255,
            -255, 1 << 20, -(1 << 20), 0, 0, 1, -1, 0,
        ];
        let right: Vec<i32> = left.iter().map(|s| s.wrapping_neg()).collect();
        let expected = [left, right];
        let source = MemorySource {
            data: std::io::Cursor::new(flac_fixture(32, &expected)),
        };
        let mut decoder = SymphoniaDecoder::open(Box::new(source), Some("flac")).unwrap();
        assert_eq!(decoder.spec().encoding, SampleEncoding::S32);
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.frames, 16);
        match block.pcm {
            DecodedPcm::I32(samples) => {
                let want: Vec<i32> = expected[0]
                    .iter()
                    .zip(&expected[1])
                    .flat_map(|(&l, &r)| [l, r])
                    .collect();
                assert_eq!(samples, want.as_slice());
            }
            other => panic!("expected s32 PCM, got {:?}", other.encoding()),
        }
    }

    #[test]
    fn refine_encoding_only_narrows_declared_sub32_depths() {
        assert_eq!(refine_encoding(SampleEncoding::S32, Some(16)), SampleEncoding::S16);
        assert_eq!(refine_encoding(SampleEncoding::S32, Some(24)), SampleEncoding::S24);
        for bits in [None, Some(8), Some(20), Some(32)] {
            assert_eq!(refine_encoding(SampleEncoding::S32, bits), SampleEncoding::S32);
        }
        assert_eq!(refine_encoding(SampleEncoding::S16, Some(16)), SampleEncoding::S16);
        assert_eq!(refine_encoding(SampleEncoding::F32, Some(24)), SampleEncoding::F32);
    }
}
