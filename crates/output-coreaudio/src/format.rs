//! Pure, platform-independent physical-format matching.
//!
//! This module deliberately knows nothing about CoreAudio FFI: it works on a
//! plain [`CandidateFormat`] mirror of `AudioStreamBasicDescription` so the
//! exact-match logic can be unit-tested on any host (including Windows, where
//! the rest of this crate is cfg'd out).

use sointty_core::{DeviceFormat, SampleEncoding, StreamSpec};

/// `kAudioFormatLinearPCM` ('lpcm'). Only linear PCM is bit-perfect capable.
pub(crate) const K_AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6c70_636d;

/// `kAudioFormatFlagIsFloat`
pub(crate) const FLAG_IS_FLOAT: u32 = 1 << 0;
/// `kAudioFormatFlagIsBigEndian`
pub(crate) const FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;
/// `kAudioFormatFlagIsSignedInteger`
pub(crate) const FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
/// `kAudioFormatFlagIsPacked`
pub(crate) const FLAG_IS_PACKED: u32 = 1 << 3;
/// `kAudioFormatFlagIsAlignedHigh`
pub(crate) const FLAG_IS_ALIGNED_HIGH: u32 = 1 << 4;
/// `kAudioFormatFlagIsNonInterleaved`
pub(crate) const FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// Platform-independent mirror of the `AudioStreamRangedDescription` fields
/// we match on. The macOS HAL layer converts the real struct into this.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CandidateFormat {
    /// Inclusive supported sample-rate range. A single fixed rate has
    /// `rate_min == rate_max`.
    pub rate_min: f64,
    pub rate_max: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
}

impl CandidateFormat {
    fn bytes_per_sample(self) -> u32 {
        if self.channels_per_frame == 0 {
            0
        } else {
            self.bytes_per_frame / self.channels_per_frame
        }
    }

    /// Common invariants for every bit-perfect candidate: linear PCM, native
    /// (little) endian, interleaved (the ring carries interleaved frames),
    /// exact channel count, and the exact sample rate inside the offered
    /// range. The HAL sets the requested rate on the physical format and
    /// verifies it by readback, so a range match is still an exact-rate
    /// guarantee.
    fn base_matches(self, spec: &StreamSpec) -> bool {
        let rate = f64::from(spec.rate_hz);
        self.format_id == K_AUDIO_FORMAT_LINEAR_PCM
            && self.format_flags & FLAG_IS_BIG_ENDIAN == 0
            && self.format_flags & FLAG_IS_NON_INTERLEAVED == 0
            && self.channels_per_frame == u32::from(spec.layout.channels)
            && self.rate_min <= rate
            && rate <= self.rate_max
    }
}

/// Result of an exact physical-format match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MatchedFormat {
    /// Index into the candidate list that was picked.
    pub index: usize,
    /// The device format the candidate maps to.
    pub format: DeviceFormat,
    /// Valid bits per sample (`mBitsPerChannel`), for `OutputSpec::valid_bits`.
    pub valid_bits: u8,
}

/// Map one candidate to a [`DeviceFormat`] for `spec`, or `None` if it does
/// not preserve the decoded encoding exactly.
fn classify(candidate: CandidateFormat, spec: &StreamSpec) -> Option<DeviceFormat> {
    if !candidate.base_matches(spec) {
        return None;
    }
    let flags = candidate.format_flags;
    let signed_int = flags & FLAG_IS_SIGNED_INTEGER != 0;
    let float = flags & FLAG_IS_FLOAT != 0;
    let packed = flags & FLAG_IS_PACKED != 0;
    let aligned_high = flags & FLAG_IS_ALIGNED_HIGH != 0;
    let bits = candidate.bits_per_channel;
    let container_bits = candidate.bytes_per_sample() * 8;

    match spec.encoding {
        SampleEncoding::S16 => {
            if signed_int && !float && packed && bits == 16 && container_bits == 16 {
                Some(DeviceFormat::S16Le)
            } else {
                None
            }
        }
        SampleEncoding::S24 if signed_int && !float => {
            // Core Audio's canonical 24-bit container is 24-in-32 aligned
            // high. A signed 32-bit slot also preserves a 24-bit stream
            // exactly when samples are written top-aligned with zero low
            // bits: the exact integer widening the fidelity contract
            // permits, and the only option on devices (e.g. some USB DACs)
            // that offer no 24-bit container.
            //
            // Packed 3-byte 24-bit formats are deliberately NOT matched:
            // drivers accept the physical-format property but the HAL IO
            // path keeps running 4-byte frames, so playback comes out
            // garbled and fast (observed on an iFi USB DAC).
            match (bits, container_bits, aligned_high) {
                (24, 32, true) | (32, 32, _) => Some(DeviceFormat::S24In32High),
                (24, 32, false) => Some(DeviceFormat::S24In32Low),
                _ => None,
            }
        }
        SampleEncoding::S32 => {
            if signed_int && !float && packed && bits == 32 && container_bits == 32 {
                Some(DeviceFormat::S32Le)
            } else {
                None
            }
        }
        SampleEncoding::F32 => {
            if float && !signed_int && bits == 32 && container_bits == 32 {
                Some(DeviceFormat::F32Le)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Pick the offered physical format that preserves `spec` exactly.
///
/// For 24-bit input the preference order is a high-aligned container
/// (canonical 24-in-32 or a widened 32-bit slot), then (least preferred,
/// but still exact) low-aligned 24-in-32.
pub(crate) fn pick_physical_format(
    candidates: &[CandidateFormat],
    spec: &StreamSpec,
) -> Option<MatchedFormat> {
    fn try_pick(candidates: &[CandidateFormat], spec: &StreamSpec) -> Option<MatchedFormat> {
        candidates.iter().enumerate().find_map(|(index, &c)| {
            classify(c, spec).map(|format| MatchedFormat {
                index,
                format,
                valid_bits: c.bits_per_channel as u8,
            })
        })
    }

    match spec.encoding {
        SampleEncoding::S24 => {
            let wanted = [DeviceFormat::S24In32High, DeviceFormat::S24In32Low];
            wanted.iter().find_map(|&format| {
                candidates.iter().enumerate().find_map(|(index, &c)| {
                    if classify(c, spec) == Some(format) {
                        Some(MatchedFormat {
                            index,
                            format,
                            // The stream's 24 valid bits, not the container
                            // width (which may be a widened 32-bit slot).
                            valid_bits: 24,
                        })
                    } else {
                        None
                    }
                })
            })
        }
        _ => try_pick(candidates, spec),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sointty_core::ChannelLayout;

    const SIGNED_PACKED: u32 = FLAG_IS_SIGNED_INTEGER | FLAG_IS_PACKED;

    fn spec(rate_hz: u32, channels: u16, encoding: SampleEncoding) -> StreamSpec {
        StreamSpec {
            rate_hz,
            layout: ChannelLayout::discrete(channels),
            encoding,
        }
    }

    fn lpcm(rate: f64, channels: u32, flags: u32, bytes_per_frame: u32, bits: u32) -> CandidateFormat {
        lpcm_ranged(rate, rate, channels, flags, bytes_per_frame, bits)
    }

    fn lpcm_ranged(
        rate_min: f64,
        rate_max: f64,
        channels: u32,
        flags: u32,
        bytes_per_frame: u32,
        bits: u32,
    ) -> CandidateFormat {
        CandidateFormat {
            rate_min,
            rate_max,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: flags,
            bytes_per_frame,
            channels_per_frame: channels,
            bits_per_channel: bits,
        }
    }

    #[test]
    fn picks_exact_s16() {
        let offered = vec![
            lpcm(48000.0, 2, SIGNED_PACKED, 4, 16),
            lpcm(44100.0, 2, SIGNED_PACKED, 4, 16),
        ];
        let m = pick_physical_format(&offered, &spec(44100, 2, SampleEncoding::S16)).unwrap();
        assert_eq!(m.index, 1);
        assert_eq!(m.format, DeviceFormat::S16Le);
        assert_eq!(m.valid_bits, 16);
    }

    #[test]
    fn picks_exact_f32() {
        let offered = vec![lpcm(96000.0, 2, FLAG_IS_FLOAT | FLAG_IS_PACKED, 8, 32)];
        let m = pick_physical_format(&offered, &spec(96000, 2, SampleEncoding::F32)).unwrap();
        assert_eq!(m.format, DeviceFormat::F32Le);
        assert_eq!(m.valid_bits, 32);
    }

    #[test]
    fn picks_exact_s32() {
        let offered = vec![lpcm(192000.0, 2, SIGNED_PACKED, 8, 32)];
        let m = pick_physical_format(&offered, &spec(192000, 2, SampleEncoding::S32)).unwrap();
        assert_eq!(m.format, DeviceFormat::S32Le);
    }

    #[test]
    fn s24_prefers_high_aligned_in32() {
        let offered = vec![
            lpcm(44100.0, 2, SIGNED_PACKED, 6, 24), // true packed 24
            lpcm(44100.0, 2, FLAG_IS_SIGNED_INTEGER | FLAG_IS_ALIGNED_HIGH, 8, 24), // 24-in-32 high
        ];
        let m = pick_physical_format(&offered, &spec(44100, 2, SampleEncoding::S24)).unwrap();
        assert_eq!(m.index, 1);
        assert_eq!(m.format, DeviceFormat::S24In32High);
        assert_eq!(m.valid_bits, 24);
    }

    #[test]
    fn s24_rejects_packed24_only_device() {
        // Packed 3-byte formats are not matched on macOS: drivers accept
        // the property but the IO path keeps running 4-byte frames.
        let offered = vec![lpcm(44100.0, 2, SIGNED_PACKED, 6, 24)];
        assert!(pick_physical_format(&offered, &spec(44100, 2, SampleEncoding::S24)).is_none());
    }

    #[test]
    fn s24_widens_exactly_into_s32_slot() {
        // Devices without a 24-bit container (iFi USB DAC): the signed
        // 32-bit slot carries the samples top-aligned, low byte zero.
        let offered = vec![
            lpcm(44100.0, 2, SIGNED_PACKED, 6, 24), // packed 24, ignored
            lpcm(44100.0, 2, SIGNED_PACKED, 8, 32),
        ];
        let m = pick_physical_format(&offered, &spec(44100, 2, SampleEncoding::S24)).unwrap();
        assert_eq!(m.index, 1);
        assert_eq!(m.format, DeviceFormat::S24In32High);
        assert_eq!(m.valid_bits, 24);
    }


    #[test]
    fn s24_low_aligned_last_resort() {
        let offered = vec![lpcm(44100.0, 2, FLAG_IS_SIGNED_INTEGER, 8, 24)];
        let m = pick_physical_format(&offered, &spec(44100, 2, SampleEncoding::S24)).unwrap();
        assert_eq!(m.format, DeviceFormat::S24In32Low);
    }

    #[test]
    fn matches_ranged_listing_containing_the_exact_rate() {
        // kAudioStreamAnyRate hardware offers min/max ranges; the HAL pins
        // the exact rate on the physical format and verifies the readback.
        let offered = vec![lpcm_ranged(44100.0, 192000.0, 2, SIGNED_PACKED, 4, 16)];
        let m = pick_physical_format(&offered, &spec(96000, 2, SampleEncoding::S16)).unwrap();
        assert_eq!(m.format, DeviceFormat::S16Le);
    }

    #[test]
    fn rejects_range_excluding_the_rate() {
        let offered = vec![lpcm_ranged(44100.0, 48000.0, 2, SIGNED_PACKED, 4, 16)];
        assert!(pick_physical_format(&offered, &spec(96000, 2, SampleEncoding::S16)).is_none());
    }

    #[test]
    fn rejects_rate_channel_and_encoding_mismatches() {
        let s = spec(44100, 2, SampleEncoding::S16);
        // wrong rate
        assert!(pick_physical_format(&[lpcm(48000.0, 2, SIGNED_PACKED, 4, 16)], &s).is_none());
        // wrong channels
        assert!(pick_physical_format(&[lpcm(44100.0, 6, SIGNED_PACKED, 12, 16)], &s).is_none());
        // float where int wanted
        assert!(pick_physical_format(&[lpcm(44100.0, 2, FLAG_IS_FLOAT | FLAG_IS_PACKED, 8, 32)], &s).is_none());
        // wrong bit depth
        assert!(pick_physical_format(&[lpcm(44100.0, 2, SIGNED_PACKED, 8, 32)], &s).is_none());
        // non-PCM (e.g. AC-3 passthrough entry)
        let ac3 = CandidateFormat {
            format_id: 0x61632d33, // 'ac-3'
            ..lpcm(44100.0, 2, SIGNED_PACKED, 4, 16)
        };
        assert!(pick_physical_format(&[ac3], &s).is_none());
    }

    #[test]
    fn rejects_big_endian_and_non_interleaved() {
        let s = spec(44100, 2, SampleEncoding::S16);
        let be = CandidateFormat {
            format_flags: FLAG_IS_SIGNED_INTEGER | FLAG_IS_PACKED | FLAG_IS_BIG_ENDIAN,
            ..lpcm(44100.0, 2, SIGNED_PACKED, 4, 16)
        };
        assert!(pick_physical_format(&[be], &s).is_none());
        let ni = CandidateFormat {
            format_flags: FLAG_IS_SIGNED_INTEGER | FLAG_IS_PACKED | FLAG_IS_NON_INTERLEAVED,
            ..lpcm(44100.0, 2, SIGNED_PACKED, 4, 16)
        };
        assert!(pick_physical_format(&[ni], &s).is_none());
    }
}
