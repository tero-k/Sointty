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

pub struct SymphoniaDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    spec: StreamSpec,
    position: u64,
    block_i16: Vec<i16>,
    block_i24: Vec<i32>,
    block_i32: Vec<i32>,
    block_f32: Vec<f32>,
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

        let (track_id, codec_params) = {
            let track = format
                .default_track(TrackType::Audio)
                .or_else(|| format.first_track_known_codec(TrackType::Audio))
                .ok_or(PlayerError::Decode)?;
            let codec_params: AudioCodecParameters = track
                .codec_params
                .clone()
                .and_then(|params| params.audio().cloned())
                .ok_or(PlayerError::Decode)?;
            (track.id, codec_params)
        };
        let decoder = (symphonia::default::get_codecs()
            .get_audio_decoder(codec_params.codec)
            .ok_or(PlayerError::Decode)?
            .factory)(
            &codec_params,
            &AudioDecoderOptions::default().gapless(true).verify(false),
        )
        .map_err(|_| PlayerError::Decode)?;

        let spec = spec_from_params(&codec_params)?;
        validate_exact_spec(&spec)?;
        Ok(Self {
            format,
            decoder,
            track_id,
            spec,
            position: 0,
            block_i16: Vec::new(),
            block_i24: Vec::new(),
            block_i32: Vec::new(),
            block_f32: Vec::new(),
        })
    }
}

impl Decoder for SymphoniaDecoder {
    fn spec(&self) -> StreamSpec {
        self.spec
    }

    fn next_block(&mut self) -> Result<Option<DecodedBlock<'_>>, PlayerError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(None),
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
            let spec = match decoded.spec().rate() {
                rate if rate == self.spec.rate_hz => self.spec,
                _ => return Err(PlayerError::Decode),
            };
            self.position += frames as u64;

            match decoded {
                symphonia::core::audio::GenericAudioBufferRef::S16(buffer) => {
                    self.block_i16.clear();
                    self.block_i16.reserve(buffer.samples_interleaved());
                    buffer.copy_to_vec_interleaved(&mut self.block_i16);
                    return Ok(Some(DecodedBlock::pcm_block(
                        spec,
                        frames as u32,
                        DecodedPcm::I16(&self.block_i16),
                    )));
                }
                symphonia::core::audio::GenericAudioBufferRef::S24(buffer) => {
                    self.block_i24.clear();
                    self.block_i24.reserve(buffer.samples_interleaved());
                    for plane_index in 0..buffer.num_planes() {
                        let plane = buffer.plane(plane_index).ok_or(PlayerError::Decode)?;
                        for &sample in plane {
                            self.block_i24.push(sample.inner());
                        }
                    }
                    return Ok(Some(DecodedBlock::pcm_block(
                        spec,
                        frames as u32,
                        DecodedPcm::I24(&self.block_i24),
                    )));
                }
                symphonia::core::audio::GenericAudioBufferRef::S32(buffer) => {
                    self.block_i32.clear();
                    self.block_i32.reserve(buffer.samples_interleaved());
                    buffer.copy_to_vec_interleaved(&mut self.block_i32);
                    return Ok(Some(DecodedBlock::pcm_block(
                        spec,
                        frames as u32,
                        DecodedPcm::I32(&self.block_i32),
                    )));
                }
                symphonia::core::audio::GenericAudioBufferRef::F32(buffer) => {
                    self.block_f32.clear();
                    self.block_f32.reserve(buffer.samples_interleaved());
                    buffer.copy_to_vec_interleaved(&mut self.block_f32);
                    return Ok(Some(DecodedBlock::pcm_block(
                        spec,
                        frames as u32,
                        DecodedPcm::F32(&self.block_f32),
                    )));
                }
                _ => return Err(PlayerError::Decode),
            }
        }
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
        self.position = seeked.actual_ts.get() as u64;
        Ok(self.position)
    }
}

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
}
