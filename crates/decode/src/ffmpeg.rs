//! Optional streaming FFmpeg adapter. No resampling, mixing or PCM conversion.
//! `StreamIo` wraps our bounded, cancellable `Source` without loading a file.
//! FFmpeg's DSF demuxer reports bytes/channel/s and returns planar DSD packets;
//! we interleave and (for LSBF) reverse bits, never invoke the DSD-to-PCM decoder.

use std::ffi::CString;

use ffmpeg_next as ffmpeg;
use ffmpeg::format::{self, context::StreamIo};
use ffmpeg::format::sample::Type;
use sointty_core::{
    ChannelLayout, DecodedBlock, DecodedPcm, DecodedSpec, Decoder, DsdSpec, PlayerError,
    SampleEncoding, Source, StreamSpec, validate_exact_spec,
};

#[derive(Debug, Clone, Copy)]
pub struct FormatAvailability {
    pub extension: &'static str,
    pub demuxer: bool,
    pub decoder: bool,
}

/// Check the linked FFmpeg build, not an assumed compile-time support list.
/// DSF/DFF are demux-only: `decoder` is true when DSD packets can pass through.
/// Stock FFmpeg 9 has `dsf`, but no DSDIFF/DFF demuxer.
pub fn availability() -> Vec<FormatAvailability> {
    use ffmpeg::codec::Id;
    [
        ("ape", "ape", Some(Id::APE)),
        ("wv", "wv", Some(Id::WAVPACK)),
        ("tak", "tak", Some(Id::TAK)),
        ("mpc", "mpc", None),
        ("dsf", "dsf", None),
        ("dff", "dsdiff", None),
    ]
    .into_iter()
    .map(|(extension, demuxer, codec)| {
        let name = CString::new(demuxer).expect("static format name");
        let demuxer = unsafe { !ffmpeg::ffi::av_find_input_format(name.as_ptr()).is_null() };
        let decoder = match extension {
            "mpc" => ffmpeg::decoder::find(Id::MUSEPACK7).is_some()
                || ffmpeg::decoder::find(Id::MUSEPACK8).is_some(),
            "dsf" | "dff" => true, // raw DSD packets, no decoder or PCM conversion
            _ => codec.is_some_and(|id| ffmpeg::decoder::find(id).is_some()),
        };
        FormatAvailability {
            extension,
            demuxer,
            decoder,
        }
    })
    .collect()
}

fn unavailable(extension: &str) -> PlayerError {
    PlayerError::InvalidInput(match extension {
        "ape" => "FFmpeg APE demuxer or decoder unavailable",
        "wv" => "FFmpeg WavPack demuxer or decoder unavailable",
        "tak" => "FFmpeg TAK demuxer or decoder unavailable",
        "mpc" => "FFmpeg Musepack demuxer or decoder unavailable",
        "dsf" => "FFmpeg DSF demuxer unavailable",
        "dff" => "FFmpeg DFF demuxer unavailable",
        _ => "FFmpeg format unavailable",
    })
}

enum Mode {
    Pcm {
        decoder: ffmpeg::codec::decoder::Audio,
        eof_sent: bool,
    },
    Dsd {
        spec: DsdSpec,
        planar: bool,
        reverse_bits: bool,
    },
}

pub struct FfmpegDecoder {
    input: format::context::Input,
    stream_index: usize,
    mode: Mode,
    spec: StreamSpec,
    frame: ffmpeg::frame::Audio,
    i16_data: Vec<i16>,
    i32_data: Vec<i32>,
    f32_data: Vec<f32>,
    dsd_data: Vec<u8>,
    pending: bool,
    pending_frames: usize,
    pending_offset: usize,
    position: u64,
}

impl FfmpegDecoder {
    pub fn open(source: Box<dyn Source>, extension: &str) -> Result<Self, PlayerError> {
        ffmpeg::init().map_err(|_| PlayerError::Decode)?;
        // The feature promises LGPL linking. A GPL/nonfree system build must
        // not silently enter a dual-licensed release.
        if !ffmpeg::format::license().starts_with("LGPL")
            || !ffmpeg::codec::license().starts_with("LGPL")
        {
            return Err(PlayerError::InvalidInput(
                "linked FFmpeg build is not LGPL; ffmpeg-lgpl refuses it",
            ));
        }
        if let Some(cap) = availability().into_iter().find(|cap| cap.extension == extension) {
            if !cap.demuxer || !cap.decoder {
                return Err(unavailable(extension));
            }
        }
        let io = StreamIo::from_read_seek(source).map_err(|_| PlayerError::Decode)?;
        let filename = format!("source.{extension}");
        let input = format::input_from_stream(io, Some(&filename), None)
            .map_err(|_| PlayerError::Decode)?;
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Audio)
            .ok_or(PlayerError::Decode)?;
        let stream_index = stream.index();
        let parameters = stream.parameters();
        let codec_id = parameters.id();
        let (layout, rate) = unsafe {
            let params = &*parameters.as_ptr();
            let ch = &params.ch_layout;
            if ch.order != ffmpeg::ffi::AVChannelOrder::AV_CHANNEL_ORDER_NATIVE
                || ch.nb_channels < 1
                || ch.nb_channels > 8
            {
                return Err(PlayerError::InvalidInput(
                    "FFmpeg stream has absent or ambiguous channel layout",
                ));
            }
            let mask = ch.u.mask as u32;
            if mask.count_ones() != ch.nb_channels as u32 || params.sample_rate <= 0 {
                return Err(PlayerError::InvalidInput(
                    "FFmpeg stream has inconsistent channel layout or sample rate",
                ));
            }
            (ChannelLayout::new(ch.nb_channels as u16, mask), params.sample_rate as u32)
        };
        let is_dsd = matches!(
            codec_id,
            ffmpeg::codec::Id::DSD_LSBF
                | ffmpeg::codec::Id::DSD_MSBF
                | ffmpeg::codec::Id::DSD_LSBF_PLANAR
                | ffmpeg::codec::Id::DSD_MSBF_PLANAR
        );
        let mode = if is_dsd {
            let dsd_rate_hz = rate.checked_mul(8).ok_or(PlayerError::Decode)?;
            Mode::Dsd {
                spec: DsdSpec { dsd_rate_hz, layout },
                planar: matches!(
                    codec_id,
                    ffmpeg::codec::Id::DSD_LSBF_PLANAR | ffmpeg::codec::Id::DSD_MSBF_PLANAR
                ),
                reverse_bits: matches!(
                    codec_id,
                    ffmpeg::codec::Id::DSD_LSBF | ffmpeg::codec::Id::DSD_LSBF_PLANAR
                ),
            }
        } else {
            if extension == "dsf" || extension == "dff" {
                return Err(PlayerError::InvalidInput(
                    "FFmpeg demuxer did not expose raw DSD packets",
                ));
            }
            if ffmpeg::decoder::find(codec_id).is_none() {
                return Err(unavailable(extension));
            }
            let ctx = ffmpeg::codec::Context::from_parameters(parameters)
                .map_err(|_| PlayerError::Decode)?;
            let decoder = ctx.decoder().audio().map_err(|_| unavailable(extension))?;
            Mode::Pcm { decoder, eof_sent: false }
        };
        let mut result = Self {
            input,
            stream_index,
            mode,
            spec: StreamSpec {
                rate_hz: rate,
                layout,
                encoding: SampleEncoding::Dsd, // replaced by first PCM frame when needed
            },
            frame: ffmpeg::frame::Audio::empty(),
            i16_data: Vec::new(),
            i32_data: Vec::new(),
            f32_data: Vec::new(),
            dsd_data: Vec::new(),
            pending: false,
            pending_frames: 0,
            pending_offset: 0,
            position: 0,
        };
        if matches!(result.mode, Mode::Pcm { .. }) {
            if !result.decode_next()? {
                return Err(PlayerError::Decode);
            }
            result.pending = true;
        }
        Ok(result)
    }

    fn decode_next(&mut self) -> Result<bool, PlayerError> {
        self.pending_offset = 0;
        loop {
            match &mut self.mode {
                Mode::Dsd { spec, planar, reverse_bits } => {
                    let mut packet = ffmpeg::Packet::empty();
                    match packet.read(&mut self.input) {
                        Ok(()) if packet.stream() != self.stream_index => continue,
                        Ok(()) => {
                            let data = packet.data().ok_or(PlayerError::Decode)?;
                            let channels = spec.layout.channels as usize;
                            if data.is_empty() || data.len() % channels != 0 {
                                return Err(PlayerError::Decode);
                            }
                            let frames = data.len() / channels;
                            self.dsd_data.clear();
                            self.dsd_data.reserve(data.len());
                            if *planar {
                                for frame in 0..frames {
                                    for channel in 0..channels {
                                        let byte = data[channel * frames + frame];
                                        self.dsd_data.push(if *reverse_bits { byte.reverse_bits() } else { byte });
                                    }
                                }
                            } else {
                                self.dsd_data.extend(data.iter().map(|byte| {
                                    if *reverse_bits { byte.reverse_bits() } else { *byte }
                                }));
                            }
                            self.pending_frames = frames;
                            return Ok(true);
                        }
                        Err(ffmpeg::Error::Eof) => return Ok(false),
                        Err(_) => return Err(PlayerError::Decode),
                    }
                }
                Mode::Pcm { decoder, eof_sent } => {
                    match decoder.receive_frame(&mut self.frame) {
                        Ok(()) => {
                            let frames = self.frame.samples();
                            if frames == 0 { continue; }
                            let encoding = match self.frame.format() {
                                format::Sample::I16(_) => SampleEncoding::S16,
                                format::Sample::I32(_) => SampleEncoding::S32,
                                format::Sample::F32(_) => SampleEncoding::F32,
                                _ => return Err(PlayerError::UnsupportedFormat {
                                    rate_hz: self.spec.rate_hz,
                                    channels: self.spec.layout.channels,
                                    encoding: self.spec.encoding,
                                    reason: "FFmpeg decoded sample format is not exact S16/S32/F32",
                                }),
                            };
                            if self.spec.encoding != SampleEncoding::Dsd
                                && (encoding != self.spec.encoding
                                    || self.frame.rate() != self.spec.rate_hz
                                    || self.frame.channels() != self.spec.layout.channels)
                            {
                                return Err(PlayerError::Decode);
                            }
                            self.spec.encoding = encoding;
                            self.spec.rate_hz = self.frame.rate();
                            validate_exact_spec(&self.spec)?;
                            self.copy_pcm(frames)?;
                            self.pending_frames = frames;
                            return Ok(true);
                        }
                        Err(ffmpeg::Error::Eof) => return Ok(false),
                        Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                            if *eof_sent { return Err(PlayerError::Decode); }
                            let mut packet = ffmpeg::Packet::empty();
                            match packet.read(&mut self.input) {
                                Ok(()) if packet.stream() != self.stream_index => continue,
                                Ok(()) => decoder.send_packet(&packet).map_err(|_| PlayerError::Decode)?,
                                Err(ffmpeg::Error::Eof) => {
                                    decoder.send_eof().map_err(|_| PlayerError::Decode)?;
                                    *eof_sent = true;
                                }
                                Err(_) => return Err(PlayerError::Decode),
                            }
                        }
                        Err(_) => return Err(PlayerError::Decode),
                    }
                }
            }
        }
    }

    fn copy_pcm(&mut self, frames: usize) -> Result<(), PlayerError> {
        let channels = self.spec.layout.channels as usize;
        let samples = frames.checked_mul(channels).ok_or(PlayerError::Decode)?;
        match self.frame.format() {
            format::Sample::I16(Type::Packed) => {
                self.i16_data.clear();
                self.i16_data.extend_from_slice(self.frame.plane::<i16>(0));
                if self.i16_data.len() != samples { return Err(PlayerError::Decode); }
            }
            format::Sample::I16(Type::Planar) => {
                self.i16_data.resize(samples, 0);
                for channel in 0..channels {
                    for (frame, &sample) in self.frame.plane::<i16>(channel).iter().enumerate() {
                        self.i16_data[frame * channels + channel] = sample;
                    }
                }
            }
            format::Sample::I32(Type::Packed) => {
                self.i32_data.clear();
                self.i32_data.extend_from_slice(self.frame.plane::<i32>(0));
                if self.i32_data.len() != samples { return Err(PlayerError::Decode); }
            }
            format::Sample::I32(Type::Planar) => {
                self.i32_data.resize(samples, 0);
                for channel in 0..channels {
                    for (frame, &sample) in self.frame.plane::<i32>(channel).iter().enumerate() {
                        self.i32_data[frame * channels + channel] = sample;
                    }
                }
            }
            format::Sample::F32(Type::Packed) => {
                self.f32_data.clear();
                self.f32_data.extend_from_slice(self.frame.plane::<f32>(0));
                if self.f32_data.len() != samples { return Err(PlayerError::Decode); }
            }
            format::Sample::F32(Type::Planar) => {
                self.f32_data.resize(samples, 0.0);
                for channel in 0..channels {
                    for (frame, &sample) in self.frame.plane::<f32>(channel).iter().enumerate() {
                        self.f32_data[frame * channels + channel] = sample;
                    }
                }
            }
            _ => return Err(PlayerError::Decode),
        }
        Ok(())
    }
}

impl Decoder for FfmpegDecoder {
    fn spec(&self) -> StreamSpec { self.spec }

    fn dsd_spec(&self) -> Option<DsdSpec> {
        match self.mode {
            Mode::Dsd { spec, .. } => Some(spec),
            Mode::Pcm { .. } => None,
        }
    }

    fn next_block(&mut self) -> Result<Option<DecodedBlock<'_>>, PlayerError> {
        if !self.pending && !self.decode_next()? { return Ok(None); }
        self.pending = false;
        let offset = self.pending_offset;
        let frames = self.pending_frames - offset;
        self.position += frames as u64;
        self.pending_offset = 0;
        let channels = self.spec.layout.channels as usize;
        let pcm = match (&self.mode, self.spec.encoding) {
            (Mode::Dsd { .. }, _) => DecodedPcm::Dsd(&self.dsd_data[offset * channels..]),
            (_, SampleEncoding::S16) => DecodedPcm::I16(&self.i16_data[offset * channels..]),
            (_, SampleEncoding::S32) => DecodedPcm::I32(&self.i32_data[offset * channels..]),
            (_, SampleEncoding::F32) => DecodedPcm::F32(&self.f32_data[offset * channels..]),
            _ => return Err(PlayerError::Decode),
        };
        let spec = match self.mode {
            Mode::Dsd { spec, .. } => DecodedSpec::Dsd(spec),
            Mode::Pcm { .. } => DecodedSpec::Pcm(self.spec),
        };
        Ok(Some(DecodedBlock::new(spec, frames as u32, pcm)))
    }

    /// Exact seek by returning to the start and decoding/discarding up to the
    /// requested frame. O(distance), but never guesses a compressed seek point
    /// or reports a frame it has not reached. DSD frames are bytes/channel.
    fn seek_to_frame(&mut self, frame: u64) -> Result<u64, PlayerError> {
        self.input.seek(0, ..).map_err(|_| PlayerError::Decode)?;
        if let Mode::Pcm { decoder, eof_sent } = &mut self.mode {
            decoder.flush();
            *eof_sent = false;
        }
        self.pending = false;
        self.pending_offset = 0;
        self.position = 0;
        while self.position < frame {
            if !self.decode_next()? { return Ok(self.position); }
            let available = self.pending_frames as u64;
            if self.position + available <= frame {
                self.position += available;
            } else {
                self.pending_offset = (frame - self.position) as usize;
                self.pending = true;
                self.position = frame;
            }
        }
        Ok(self.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct MemorySource(Cursor<Vec<u8>>);
    impl Read for MemorySource {
        fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(dst)
        }
    }
    impl Seek for MemorySource {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }
    impl Source for MemorySource {
        fn size_hint(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
    }

    fn dsf_fixture() -> Vec<u8> {
        let channel_bytes = 4096usize;
        let data_size = 2 * channel_bytes;
        let mut bytes = Vec::with_capacity(28 + 52 + 12 + data_size);
        bytes.extend_from_slice(b"DSD ");
        bytes.extend_from_slice(&28u64.to_le_bytes());
        bytes.extend_from_slice(&((28 + 52 + 12 + data_size) as u64).to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // no ID3
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&52u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // format version
        bytes.extend_from_slice(&0u32.to_le_bytes()); // format id
        bytes.extend_from_slice(&2u32.to_le_bytes()); // stereo
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&2_822_400u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes()); // LSBF planar
        bytes.extend_from_slice(&((channel_bytes * 8) as u64).to_le_bytes());
        bytes.extend_from_slice(&(channel_bytes as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&((data_size + 12) as u64).to_le_bytes());
        for i in 0..channel_bytes {
            bytes.push((i as u8).wrapping_add(1));
        }
        for i in 0..channel_bytes {
            bytes.push(0x80 ^ i as u8);
        }
        bytes
    }

    #[test]
    fn reports_all_optional_formats_and_typed_unavailability() {
        let caps = availability();
        assert_eq!(caps.iter().map(|c| c.extension).collect::<Vec<_>>(),
            ["ape", "wv", "tak", "mpc", "dsf", "dff"]);
        for cap in caps {
            let source = Box::new(MemorySource(Cursor::new(Vec::new())));
            let result = FfmpegDecoder::open(source, cap.extension);
            if !cap.demuxer || !cap.decoder {
                assert!(matches!(result, Err(PlayerError::InvalidInput(_))),
                    "{} must fail with a typed missing-format error", cap.extension);
            } else {
                assert!(matches!(result, Err(PlayerError::Decode)),
                    "{} invalid data must fail as Decode, not unavailable", cap.extension);
            }
        }
    }

    #[test]
    fn dsf_demux_keeps_raw_bits_and_seeks_in_dsd_byte_units() {
        if !availability().iter().any(|cap| cap.extension == "dsf" && cap.demuxer) {
            return; // a restricted LGPL build can omit this demuxer
        }
        let mut decoder = FfmpegDecoder::open(
            Box::new(MemorySource(Cursor::new(dsf_fixture()))),
            "dsf",
        ).unwrap();
        assert_eq!(decoder.dsd_spec().unwrap().dsd_rate_hz, 2_822_400);
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.frames, 4096);
        assert!(matches!(block.spec, DecodedSpec::Dsd(_)));
        let DecodedPcm::Dsd(data) = block.pcm else { panic!("not DSD") };
        assert_eq!(&data[..4], &[1u8.reverse_bits(), 0x80u8.reverse_bits(),
            2u8.reverse_bits(), 0x81u8.reverse_bits()]);
        assert!(decoder.next_block().unwrap().is_none());
        assert_eq!(decoder.seek_to_frame(7).unwrap(), 7);
        let block = decoder.next_block().unwrap().unwrap();
        assert_eq!(block.frames, 4089);
        let DecodedPcm::Dsd(data) = block.pcm else { panic!("not DSD") };
        assert_eq!(&data[..2], &[8u8.reverse_bits(), (0x80u8 ^ 7).reverse_bits()]);
    }
    #[test]
    fn wavpack_fixture_decodes_streaming_pcm() {
        let bytes = include_bytes!("fixtures/wv-4096.wv");
        let mut decoder = FfmpegDecoder::open(
            Box::new(MemorySource(Cursor::new(bytes.to_vec()))), "wv"
        ).unwrap();
        let spec = decoder.spec();
        assert_eq!(spec.rate_hz, 44_100);
        assert_eq!(spec.layout, ChannelLayout::new(2, 3));
        assert_eq!(spec.encoding, SampleEncoding::S16);
        let first = decoder.next_block().unwrap().unwrap();
        assert_eq!(first.frames, 4096);
        let DecodedPcm::I16(samples) = first.pcm else { panic!("not S16") };
        assert_eq!(&samples[..4], &[-16000, 16000, -15689, 15803]);
        assert_eq!(samples[4095 * 2], ((4095 * 311) % 32000 - 16000) as i16);
        assert_eq!(samples[4095 * 2 + 1], (16000 - (4095 * 197) % 32000) as i16);
        assert!(decoder.next_block().unwrap().is_none());
        assert_eq!(decoder.seek_to_frame(1000).unwrap(), 1000);
        let tail = decoder.next_block().unwrap().unwrap();
        assert_eq!(tail.frames, 3096);
        let DecodedPcm::I16(samples) = tail.pcm else { panic!("not S16") };
        assert_eq!(&samples[..2], &[((1000 * 311) % 32000 - 16000) as i16,
            (16000 - (1000 * 197) % 32000) as i16]);
    }
}
