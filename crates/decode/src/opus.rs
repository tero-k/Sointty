//! Ogg Opus decoding (RFC 7845) via libopus.
//!
//! The output timeline is always 48 kHz float32; the input sample rate from
//! the OpusHead header is ignored, per RFC 7845 (the decoder runs at 48 kHz
//! regardless). Nonzero output gain is rejected: applying it would violate the
//! bit-exact passthrough contract.
//!
//! Seeking uses `ogg::PacketReader::seek_absgp` to reach the page bracketing
//! the target granule, then decodes and discards packets until the exact frame
//! is reached. Frame accounting uses page granule positions, which per
//! RFC 7845 count all decoded samples including the pre-skip padding.

use std::io::BufReader;

use ogg::{OggReadError, PacketReader};
use opus::{Channels, Decoder as OpusDecoder, MSDecoder};
use sointty_core::{
    ChannelLayout, DecodedBlock, DecodedPcm, Decoder, PlayerError, SampleEncoding, Source,
    StreamSpec,
};

const RATE_HZ: u32 = 48_000;
/// Longest Opus frame (120 ms) at 48 kHz.
const MAX_FRAME: usize = 5760;
/// Consecutive packet decode failures tolerated before giving up.
const MAX_BAD_PACKETS: u32 = 8;

enum Backend {
    Plain(OpusDecoder),
    Multistream(MSDecoder),
}

impl Backend {
    fn decode_float(&mut self, input: &[u8], output: &mut [f32]) -> Result<usize, opus::Error> {
        match self {
            Self::Plain(decoder) => decoder.decode_float(input, output, false),
            Self::Multistream(decoder) => decoder.decode_float(input, output, false),
        }
    }

    fn reset(&mut self) -> Result<(), PlayerError> {
        match self {
            Self::Plain(decoder) => decoder.reset_state(),
            Self::Multistream(decoder) => decoder.reset_state(),
        }
        .map_err(|_| PlayerError::Decode)
    }
}

fn build_backend(channels: u16, family: u8) -> Result<Backend, PlayerError> {
    let unsupported = |reason: &'static str| PlayerError::UnsupportedFormat {
        rate_hz: RATE_HZ,
        channels,
        encoding: SampleEncoding::F32,
        reason,
    };
    match (family, channels) {
        (0 | 1, 1) => Ok(Backend::Plain(
            OpusDecoder::new(RATE_HZ, Channels::Mono).map_err(|_| PlayerError::Decode)?,
        )),
        (0 | 1, 2) => Ok(Backend::Plain(
            OpusDecoder::new(RATE_HZ, Channels::Stereo).map_err(|_| PlayerError::Decode)?,
        )),
        (0, _) => Err(unsupported("mapping family 0 allows only mono or stereo")),
        // RFC 7845 mapping family 1: Vorbis channel order, fixed stream/coupling
        // table per channel count.
        (1, 3..=8) => {
            let (streams, coupled, mapping) = match channels {
                3 => (2, 1, [0, 2, 1].as_slice()),
                4 => (2, 2, [0, 1, 2, 3].as_slice()),
                5 => (3, 2, [0, 4, 1, 2, 3].as_slice()),
                6 => (4, 2, [0, 4, 1, 2, 3, 5].as_slice()),
                7 => (4, 3, [0, 4, 1, 2, 3, 5, 6].as_slice()),
                _ => (5, 3, [0, 6, 1, 2, 3, 4, 5, 7].as_slice()),
            };
            Ok(Backend::Multistream(
                MSDecoder::new(RATE_HZ, streams, coupled, mapping)
                    .map_err(|_| PlayerError::Decode)?,
            ))
        }
        (1, _) => Err(unsupported("mapping family 1 allows 1 to 8 channels")),
        _ => Err(unsupported("unsupported Opus channel mapping family")),
    }
}

/// Speaker mask for `channels`, mirroring the symphonia/WAV conventions used
/// elsewhere in the player (mono = front center, stereo = FL|FR, ...).
fn speaker_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x0004, // FC
        2 => 0x0003, // FL FR
        3 => 0x0007, // FL FR FC
        4 => 0x0033, // FL FR RL RR
        5 => 0x0037, // FL FR FC RL RR
        6 => 0x003F, // FL FR FC LFE RL RR
        7 => 0x070F, // FL FR FC LFE RC SL SR
        8 => 0x063F, // FL FR FC LFE RL RR SL SR
        _ => 0,
    }
}

fn map_ogg_error(error: OggReadError) -> PlayerError {
    match error {
        OggReadError::ReadError(io) => PlayerError::Io(io.kind()),
        _ => PlayerError::Decode,
    }
}

fn read_packet(
    reader: &mut PacketReader<BufReader<Box<dyn Source>>>,
) -> Result<Option<ogg::Packet>, PlayerError> {
    reader.read_packet().map_err(map_ogg_error)
}

/// Parses the OpusHead packet: magic, version, channel count, pre-skip,
/// input rate (ignored), output gain (must be zero), mapping family.
fn parse_head(data: &[u8]) -> Result<(u16, u64, u8), PlayerError> {
    const HEAD_LEN: usize = 19;
    if data.len() < HEAD_LEN || &data[..8] != b"OpusHead" {
        return Err(PlayerError::Decode);
    }
    if data[8] != 1 {
        return Err(PlayerError::Decode);
    }
    let channels = data[9] as u16;
    if channels == 0 {
        return Err(PlayerError::Decode);
    }
    let pre_skip = u64::from(u16::from_le_bytes([data[10], data[11]]));
    // Bytes 12..16 are the input sample rate; ignored. The output timeline
    // is always 48 kHz.
    let gain = i16::from_le_bytes([data[16], data[17]]);
    if gain != 0 {
        return Err(PlayerError::UnsupportedFormat {
            rate_hz: RATE_HZ,
            channels,
            encoding: SampleEncoding::F32,
            reason: "nonzero Opus output gain forbidden",
        });
    }
    let family = data[18];
    if family > 1 {
        return Err(PlayerError::UnsupportedFormat {
            rate_hz: RATE_HZ,
            channels,
            encoding: SampleEncoding::F32,
            reason: "unsupported Opus channel mapping family",
        });
    }
    if family == 0 && channels > 2 {
        return Err(PlayerError::UnsupportedFormat {
            rate_hz: RATE_HZ,
            channels,
            encoding: SampleEncoding::F32,
            reason: "mapping family 0 allows only mono or stereo",
        });
    }
    if family == 1 && channels > 8 {
        return Err(PlayerError::UnsupportedFormat {
            rate_hz: RATE_HZ,
            channels,
            encoding: SampleEncoding::F32,
            reason: "mapping family 1 allows 1 to 8 channels",
        });
    }
    Ok((channels, pre_skip, family))
}

pub struct OggOpusDecoder {
    reader: PacketReader<BufReader<Box<dyn Source>>>,
    serial: u32,
    spec: StreamSpec,
    backend: Backend,
    /// Pre-skip padding (frames per channel) still to discard from the stream
    /// start. Zero after a seek: seeks land past the pre-skip region.
    pre_skip: u64,
    pending_skip: u64,
    /// Frames yielded so far on the post-trim timeline.
    position: u64,
    /// Frames decoded from the stream start, on the Ogg granule timeline
    /// (includes the pre-skip padding). Corrected to the page granule
    /// position whenever a packet ends a page.
    decoded: u64,
    /// Total valid frames on the post-trim timeline; known once the final
    /// page has been seen.
    total_valid: Option<u64>,
    ended: bool,
    bad_packets: u32,
    scratch: Vec<f32>,
    /// Samples from the packet containing a seek target, retained so the
    /// next `next_block` resumes exactly at the landed frame.
    pending: u32,
    /// Staging buffer used while decode-discarding during seeks.
    discard: Vec<f32>,
}

impl OggOpusDecoder {
    pub fn open(source: Box<dyn Source>) -> Result<Self, PlayerError> {
        let mut reader = PacketReader::new(BufReader::new(source));
        let head = read_packet(&mut reader)?.ok_or(PlayerError::Decode)?;
        let serial = head.stream_serial();
        let (channels, pre_skip, family) = parse_head(&head.data)?;
        // The second packet is OpusTags; it carries no PCM data.
        match read_packet(&mut reader)? {
            Some(tags) if &tags.data[..8] == b"OpusTags" => {}
            Some(_) | None => return Err(PlayerError::Decode),
        }
        let spec = StreamSpec {
            rate_hz: RATE_HZ,
            layout: ChannelLayout::new(channels, speaker_mask(channels)),
            encoding: SampleEncoding::F32,
        };
        Ok(Self {
            reader,
            serial,
            spec,
            backend: build_backend(channels, family)?,
            pre_skip,
            pending_skip: pre_skip,
            position: 0,
            decoded: 0,
            total_valid: None,
            ended: false,
            bad_packets: 0,
            scratch: Vec::new(),
            pending: 0,
            discard: Vec::new(),
        })
    }

    /// Decodes one packet into `self.scratch`, returning the frame count
    /// (0 if the packet was skipped as corrupt).
    fn decode_packet(&mut self, data: &[u8]) -> Result<usize, PlayerError> {
        let channels = self.spec.layout.channels as usize;
        self.scratch.clear();
        self.scratch.resize(MAX_FRAME * channels, 0.0);
        match self.backend.decode_float(data, &mut self.scratch) {
            Ok(frames) => {
                self.scratch.truncate(frames * channels);
                self.bad_packets = 0;
                Ok(frames)
            }
            Err(_) => {
                self.scratch.clear();
                self.bad_packets += 1;
                if self.bad_packets >= MAX_BAD_PACKETS {
                    return Err(PlayerError::Decode);
                }
                Ok(0)
            }
        }
    }

    /// Reads and discards packets, returning the first packet of the target
    /// stream at or after the current read position. Used after granule
    /// seeks to land exactly on the requested frame.
    fn decode_discard_to(&mut self, target_ogg: u64) -> Result<u64, PlayerError> {
        let channels = self.spec.layout.channels as usize;
        let mut decoded_here: u64 = 0;
        let mut anchor: Option<(u64, u64)> = None; // (page granule, frames decoded at that page end)
        loop {
            let packet = match read_packet(&mut self.reader)? {
                Some(packet) => packet,
                None => {
                    self.ended = true;
                    return Ok(self.total_valid.unwrap_or(0));
                }
            };
            if packet.stream_serial() != self.serial || packet.data.is_empty() {
                continue;
            }
            let frames = self.decode_packet(&packet.data)? as u64;
            if frames > 0 {
                self.discard.append(&mut self.scratch);
            }
            decoded_here += frames;
            if packet.last_in_page() || packet.last_in_stream() {
                anchor = Some((packet.absgp_page(), decoded_here));
            }
            if packet.last_in_stream() {
                self.total_valid = Some(packet.absgp_page().saturating_sub(self.pre_skip));
            }
            if let Some((granule, at_anchor)) = anchor {
                let absolute_end = granule + (decoded_here - at_anchor);
                if absolute_end >= target_ogg {
                    // The buffered samples start at granule position
                    // `granule - at_anchor`; cut everything before the target
                    // and keep the tail as the next block so decoding resumes
                    // exactly at the landed frame.
                    let absolute_start = granule - at_anchor;
                    let drop = (target_ogg - absolute_start) as usize * channels;
                    self.discard.drain(..drop);
                    let landed = target_ogg - self.pre_skip;
                    let keep = match self.total_valid {
                        Some(total) => (self.discard.len() / channels)
                            .min(total.saturating_sub(landed) as usize),
                        None => self.discard.len() / channels,
                    };
                    self.discard.truncate(keep * channels);
                    self.scratch.clear();
                    self.scratch.append(&mut self.discard);
                    self.pending = keep as u32;
                    self.decoded = absolute_end;
                    self.position = landed;
                    return Ok(landed);
                }
            }
        }
    }
}

impl Decoder for OggOpusDecoder {
    fn spec(&self) -> StreamSpec {
        self.spec
    }
    fn total_frames(&self) -> Option<u64> {
        self.total_valid
    }

    fn next_block(&mut self) -> Result<Option<DecodedBlock<'_>>, PlayerError> {
        if self.ended {
            return Ok(None);
        }
        if self.pending > 0 {
            let frames = self.pending;
            self.pending = 0;
            self.position += frames as u64;
            return Ok(Some(DecodedBlock::new(
                self.spec,
                frames,
                DecodedPcm::F32(&self.scratch),
            )));
        }
        let channels = self.spec.layout.channels as usize;
        loop {
            let packet = match read_packet(&mut self.reader)? {
                Some(packet) => packet,
                None => {
                    self.ended = true;
                    return Ok(None);
                }
            };
            if packet.stream_serial() != self.serial || packet.data.is_empty() {
                continue;
            }
            let frames = match self.decode_packet(&packet.data)? {
                0 => continue,
                frames => frames,
            };
            let block_start_ogg = self.decoded;
            self.decoded += frames as u64;
            if packet.last_in_page() {
                // Authoritative position: granulepos counts all decoded
                // samples up to the end of the page, pre-skip included.
                self.decoded = packet.absgp_page();
            }
            if packet.last_in_stream() {
                self.total_valid = Some(packet.absgp_page().saturating_sub(self.pre_skip));
            }

            // Trim the pre-skip padding from the stream start.
            let mut drop_frames = 0usize;
            if self.pending_skip > 0 {
                drop_frames = self.pending_skip.min(frames as u64) as usize;
                self.pending_skip -= drop_frames as u64;
            }
            // Trim anything past the final page's granule position.
            let block_pos = block_start_ogg.max(self.pre_skip) - self.pre_skip;
            let available = frames - drop_frames;
            let keep = match self.total_valid {
                Some(total) => available.min(total.saturating_sub(block_pos) as usize),
                None => available,
            };
            if keep == 0 {
                if self.total_valid.is_some() && self.pending_skip == 0 {
                    self.ended = true;
                    return Ok(None);
                }
                continue;
            }
            self.scratch.drain(..drop_frames * channels);
            self.scratch.truncate(keep * channels);
            self.position = block_pos + keep as u64;
            return Ok(Some(DecodedBlock::new(
                self.spec,
                keep as u32,
                DecodedPcm::F32(&self.scratch),
            )));
        }
    }

    /// Seeks so the next `next_block` yields exactly `frame` (48 kHz,
    /// post-pre-skip timeline) and returns the frame landed on.
    ///
    /// `ogg::PacketReader::seek_absgp` positions at the first page whose
    /// granule position reaches `frame + pre_skip`; packets are then decoded
    /// and discarded until that exact granule, so the landing is sample-exact
    /// even when the seek lands mid-page. This is a bisection plus a short
    /// decode-discard, not a continuous decode from stream start.
    fn seek_to_frame(&mut self, frame: u64) -> Result<u64, PlayerError> {
        let frame = match self.total_valid {
            Some(total) => frame.min(total),
            None => frame,
        };
        if frame == self.position {
            return Ok(self.position);
        }
        self.backend.reset()?;
        self.scratch.clear();
        self.discard.clear();
        self.pending_skip = 0;
        self.bad_packets = 0;

        let target_ogg = frame.saturating_add(self.pre_skip);
        let found = self
            .reader
            .seek_absgp(Some(self.serial), target_ogg)
            .map_err(map_ogg_error)?;
        if !found {
            // Target is past the last page's granule position: clamp to the
            // end of the stream.
            let found_end = self
                .reader
                .seek_absgp(Some(self.serial), u64::MAX)
                .map_err(map_ogg_error)?;
            if !found_end {
                self.ended = true;
                self.total_valid = Some(0);
                self.position = 0;
                return Ok(0);
            }
            let position = self.decode_discard_to(u64::MAX)?;
            self.ended = true;
            self.position = position;
            return Ok(position);
        }
        self.ended = false;
        self.decode_discard_to(target_ogg)
    }
}

#[cfg(all(test, feature = "opus"))]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek};

    use ogg::{PacketWriteEndInfo, PacketWriter};
    use opus::{Application, Bitrate, Encoder};

    struct MemSource(Cursor<Vec<u8>>);

    impl Read for MemSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Seek for MemSource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    impl Source for MemSource {
        fn size_hint(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
    }

    const FRAME: usize = 960; // 20 ms at 48 kHz
    const SERIAL: u32 = 0x0102_0304;

    /// Encodes `frames` samples of a deterministic 300 Hz sine into an Ogg
    /// Opus stream, returning the file bytes, the input samples (interleaved)
    /// and the encoder pre-skip written into the OpusHead.
    fn encode_fixture(channels: u16, frames: usize) -> (Vec<u8>, Vec<f32>, u64) {
        let opus_channels = match channels {
            1 => Channels::Mono,
            _ => Channels::Stereo,
        };
        let mut encoder =
            Encoder::new(RATE_HZ, opus_channels, Application::Audio).expect("encoder");
        encoder.set_bitrate(Bitrate::Bits(96_000)).expect("bitrate");
        let pre_skip = encoder.get_lookahead().expect("lookahead") as u64;

        let n_ch = channels as usize;
        let input: Vec<f32> = (0..frames * n_ch)
            .map(|i| {
                let n = i / n_ch;
                0.5 * (2.0 * std::f32::consts::PI * 300.0 * n as f32 / RATE_HZ as f32 + 0.7).sin()
            })
            .collect();

        let mut bytes = Vec::new();
        {
            let mut writer = PacketWriter::new(&mut bytes);

            let mut head = b"OpusHead".to_vec();
            head.push(1); // version
            head.push(channels as u8);
            head.extend_from_slice(&(pre_skip as u16).to_le_bytes());
            head.extend_from_slice(&RATE_HZ.to_le_bytes()); // input rate
            head.extend_from_slice(&0i16.to_le_bytes()); // output gain
            head.push(0); // mapping family 0
            writer
                .write_packet(head, SERIAL, PacketWriteEndInfo::EndPage, 0)
                .expect("head");

            let mut tags = b"OpusTags".to_vec();
            tags.extend_from_slice(&0u32.to_le_bytes()); // vendor length
            tags.extend_from_slice(&0u32.to_le_bytes()); // comment count
            writer
                .write_packet(tags, SERIAL, PacketWriteEndInfo::EndPage, 0)
                .expect("tags");

            let packets = frames / FRAME;
            let mut granule = pre_skip;
            for index in 0..packets {
                let start = index * FRAME * n_ch;
                let packet = encoder
                    .encode_vec_float(&input[start..start + FRAME * n_ch], 4000)
                    .expect("encode");
                granule += FRAME as u64;
                let end = if index + 1 == packets {
                    PacketWriteEndInfo::EndStream
                } else {
                    PacketWriteEndInfo::EndPage
                };
                writer
                    .write_packet(packet, SERIAL, end, granule)
                    .expect("packet");
            }
        }
        (bytes, input, pre_skip)
    }

    fn open(bytes: Vec<u8>) -> OggOpusDecoder {
        OggOpusDecoder::open(Box::new(MemSource(Cursor::new(bytes)))).expect("open")
    }

    fn collect(decoder: &mut OggOpusDecoder) -> (u64, Vec<f32>) {
        let mut total = 0u64;
        let mut samples = Vec::new();
        while let Some(block) = decoder.next_block().expect("block") {
            assert_eq!(block.spec.rate_hz, RATE_HZ);
            total += block.frames as u64;
            match block.pcm {
                DecodedPcm::F32(data) => samples.extend_from_slice(data),
                _ => panic!("expected f32"),
            }
        }
        (total, samples)
    }

    #[test]
    fn spec_is_48k_f32_with_standard_masks() {
        let (bytes, _, _) = encode_fixture(2, FRAME * 4);
        let decoder = open(bytes);
        assert_eq!(
            decoder.spec(),
            StreamSpec {
                rate_hz: RATE_HZ,
                layout: ChannelLayout::new(2, 0x0003),
                encoding: SampleEncoding::F32,
            }
        );

        let (bytes, _, _) = encode_fixture(1, FRAME * 4);
        let decoder = open(bytes);
        assert_eq!(
            decoder.spec(),
            StreamSpec {
                rate_hz: RATE_HZ,
                layout: ChannelLayout::new(1, 0x0004),
                encoding: SampleEncoding::F32,
            }
        );
    }

    #[test]
    fn decodes_exact_length_with_pre_skip_and_eos_trim() {
        let frames = FRAME * 20;
        let (bytes, input, pre_skip) = encode_fixture(1, frames);
        assert!(pre_skip > 0, "fixture must exercise pre-skip");
        let mut decoder = open(bytes);
        let (total, samples) = collect(&mut decoder);
        assert_eq!(total, frames as u64);
        assert_eq!(samples.len(), frames);
        // First valid decoded sample corresponds to input sample 0 (Opus is
        // lossy; allow a generous tolerance for a pure tone at 96 kbit/s).
        let expected = input[0];
        assert!(
            (samples[0] - expected).abs() < 0.05,
            "pre-skip misaligned: got {}, want ~{expected}",
            samples[0]
        );
    }

    #[test]
    fn rejects_nonzero_output_gain() {
        let (mut bytes, _, _) = encode_fixture(1, FRAME * 2);
        // Locate the OpusHead packet payload inside the first page (page
        // header + segment table precede it) and corrupt the gain field at
        // offset 16.
        let magic_at = bytes
            .windows(8)
            .position(|window| window == b"OpusHead")
            .expect("OpusHead magic");
        bytes[magic_at + 16] = 1; // gain = 1 (LE)
        bytes[magic_at + 17] = 0;
        let error = OggOpusDecoder::open(Box::new(MemSource(Cursor::new(bytes))))
            .err()
            .expect("gain must be rejected");
        assert!(matches!(
            error,
            PlayerError::UnsupportedFormat {
                rate_hz: RATE_HZ,
                channels: 1,
                encoding: SampleEncoding::F32,
                reason: "nonzero Opus output gain forbidden",
            }
        ));
    }

    #[test]
    fn seek_lands_on_exact_frame() {
        let frames = FRAME * 30;
        let (bytes, input, _) = encode_fixture(2, frames);
        let mut decoder = open(bytes);

        // Decode a little, then seek backwards and forwards.
        let first = decoder.next_block().expect("block").expect("some");
        assert!(first.frames > 0);
        drop(first);

        let target = (frames / 2) as u64;
        let landed = decoder.seek_to_frame(target).expect("seek");
        assert_eq!(landed, target);

        let block = decoder.next_block().expect("block").expect("some");
        assert_eq!(block.frames as u64, FRAME as u64);
        match block.pcm {
            DecodedPcm::F32(data) => {
                let expected = input[target as usize * 2];
                assert!(
                    (data[0] - expected).abs() < 0.05,
                    "seek misaligned: got {}, want ~{expected}",
                    data[0]
                );
            }
            _ => panic!("expected f32"),
        }

        let (rest, _) = collect(&mut decoder);
        assert_eq!(rest + FRAME as u64 + landed, frames as u64);
    }

    #[test]
    fn seek_past_end_clamps_to_end() {
        let frames = FRAME * 4;
        let (bytes, _, _) = encode_fixture(1, frames);
        let mut decoder = open(bytes);
        let landed = decoder.seek_to_frame(1_000_000).expect("seek");
        assert_eq!(landed, frames as u64);
        assert!(decoder.next_block().expect("block").is_none());
    }
}
