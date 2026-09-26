//! Pure WASAPI format-candidate generation.
//!
//! Kept free of any `windows` crate types so the negotiation data-building is
//! unit-testable without a device. The Windows-only code in `imp` converts
//! [`CandidateDesc`] into WAVEFORMATEX / WAVEFORMATEXTENSIBLE.
#![cfg_attr(not(windows), allow(dead_code))]

use sointty_core::{DeviceFormat, SampleEncoding};

/// `WAVE_FORMAT_PCM` (`WAVEFORMATEX.wFormatTag`).
pub const WAVE_FORMAT_PCM: u16 = 0x0001;
/// `WAVE_FORMAT_IEEE_FLOAT` (`WAVEFORMATEX.wFormatTag`).
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// `WAVE_FORMAT_EXTENSIBLE` (`WAVEFORMATEX.wFormatTag`).
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// `WAVEFORMATEXTENSIBLE` trailing bytes (`WAVEFORMATEX.cbSize`).
pub const WAVEFORMATEXTENSIBLE_EXTRA: u16 = 22;

/// `KSDATAFORMAT_SUBTYPE_PCM`.
pub const SUBTYPE_PCM: u128 = 0x00000001_0000_0010_8000_00AA_0038_9B71;
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`.
pub const SUBTYPE_IEEE_FLOAT: u128 = 0x00000003_0000_0010_8000_00AA_0038_9B71;

/// `SPEAKER_FRONT_LEFT`.
pub const SPEAKER_FRONT_LEFT: u32 = 0x1;
/// `SPEAKER_FRONT_RIGHT`.
pub const SPEAKER_FRONT_RIGHT: u32 = 0x2;
/// `SPEAKER_FRONT_CENTER`.
pub const SPEAKER_FRONT_CENTER: u32 = 0x4;

/// One exclusive-mode format descriptor to probe with
/// `IAudioClient::IsFormatSupported`. All fields are the EXACT stream
/// parameters; no resampling, remixing, or sample-format conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateDesc {
    pub format: DeviceFormat,
    pub valid_bits: u16,
    /// `WAVEFORMATEX.wBitsPerSample` (container size).
    pub container_bits: u16,
    /// `true` -> KSDATAFORMAT_SUBTYPE_PCM, `false` -> KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
    pub subformat_pcm: bool,
}

impl CandidateDesc {
    const fn pcm(format: DeviceFormat, bits: u16) -> Self {
        Self {
            format,
            valid_bits: bits,
            container_bits: bits,
            subformat_pcm: true,
        }
    }
}

/// Candidate descriptors for one stream, in fidelity order. Only the device's
/// native container layouts are offered: WASAPI has no low-aligned 24-in-32,
/// so S24 probes packed 24-in-3 first, then 24-in-32 (high-aligned).
pub fn candidates_for(encoding: SampleEncoding) -> &'static [CandidateDesc] {
    const S16: &[CandidateDesc] = &[CandidateDesc::pcm(DeviceFormat::S16Le, 16)];
    const S24: &[CandidateDesc] = &[
        CandidateDesc::pcm(DeviceFormat::S24_3Le, 24),
        CandidateDesc {
            format: DeviceFormat::S24In32High,
            valid_bits: 24,
            container_bits: 32,
            subformat_pcm: true,
        },
    ];
    const S32: &[CandidateDesc] = &[CandidateDesc::pcm(DeviceFormat::S32Le, 32)];
    const F32: &[CandidateDesc] = &[CandidateDesc {
        format: DeviceFormat::F32Le,
        valid_bits: 32,
        container_bits: 32,
        subformat_pcm: false,
    }];
    match encoding {
        SampleEncoding::S16 => S16,
        SampleEncoding::S24 => S24,
        SampleEncoding::S32 => S32,
        SampleEncoding::F32 => F32,
        // DSD arrives only via configure_dsd, which WASAPI does not implement.
        SampleEncoding::Dsd => &[],
    }
}

/// Resolve the channel mask for a stream layout. A zero layout mask gets the
/// default mono/stereo speaker mask; anything else is used exactly as given.
pub fn channel_mask(layout: &sointty_core::ChannelLayout) -> u32 {
    if layout.mask != 0 {
        return layout.mask;
    }
    match layout.channels {
        1 => SPEAKER_FRONT_CENTER,
        2 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
        _ => 0,
    }
}

/// `WAVEFORMATEX.nBlockAlign` for a candidate.
pub fn block_align(channels: u16, candidate: &CandidateDesc) -> u16 {
    channels * (candidate.container_bits / 8) as u16
}

/// Convert a frame count to a 100-ns duration, rounded up.
pub fn frames_to_hns(frames: u32, rate_hz: u32) -> i64 {
    ((u64::from(frames) * 10_000_000 + u64::from(rate_hz) - 1) / u64::from(rate_hz)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use sointty_core::{ChannelLayout, StreamSpec};

    #[test]
    fn s16_has_single_pcm16_candidate() {
        let cands = candidates_for(SampleEncoding::S16);
        assert_eq!(cands.len(), 1);
        let c = &cands[0];
        assert_eq!(c.format, DeviceFormat::S16Le);
        assert_eq!(c.container_bits, 16);
        assert_eq!(c.valid_bits, 16);
        assert!(c.subformat_pcm);
    }

    #[test]
    fn s24_tries_packed_24_first_then_24_in_32() {
        let cands = candidates_for(SampleEncoding::S24);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].format, DeviceFormat::S24_3Le);
        assert_eq!(cands[0].container_bits, 24);
        assert_eq!(cands[0].valid_bits, 24);
        assert_eq!(cands[1].format, DeviceFormat::S24In32High);
        assert_eq!(cands[1].container_bits, 32);
        assert_eq!(cands[1].valid_bits, 24);
        assert!(cands.iter().all(|c| c.subformat_pcm));
    }

    #[test]
    fn s32_has_single_pcm32_candidate() {
        let cands = candidates_for(SampleEncoding::S32);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].format, DeviceFormat::S32Le);
        assert_eq!(cands[0].container_bits, 32);
        assert_eq!(cands[0].valid_bits, 32);
    }

    #[test]
    fn f32_has_exactly_one_ieee_float_candidate() {
        let cands = candidates_for(SampleEncoding::F32);
        assert_eq!(cands.len(), 1);
        let c = &cands[0];
        assert_eq!(c.format, DeviceFormat::F32Le);
        assert_eq!(c.container_bits, 32);
        assert_eq!(c.valid_bits, 32);
        assert!(!c.subformat_pcm);
    }

    #[test]
    fn zero_layout_mask_gets_default_speaker_mask() {
        assert_eq!(
            channel_mask(&ChannelLayout::discrete(1)),
            SPEAKER_FRONT_CENTER
        );
        assert_eq!(
            channel_mask(&ChannelLayout::discrete(2)),
            SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        );
        assert_eq!(channel_mask(&ChannelLayout::discrete(6)), 0);
    }

    #[test]
    fn explicit_layout_mask_is_used_exactly() {
        let layout = ChannelLayout::new(6, 0x3F);
        assert_eq!(channel_mask(&layout), 0x3F);
    }

    #[test]
    fn block_align_matches_container_size() {
        let s24 = &candidates_for(SampleEncoding::S24)[1];
        assert_eq!(block_align(2, s24), 8);
        let s16 = &candidates_for(SampleEncoding::S16)[0];
        assert_eq!(block_align(2, s16), 4);
    }

    #[test]
    fn frames_to_hns_rounds_up_and_handles_rate() {
        assert_eq!(frames_to_hns(441, 44_100), 100_000);
        assert_eq!(frames_to_hns(480, 48_000), 100_000);
        assert_eq!(frames_to_hns(1, 192_000), 53);
    }

    #[test]
    fn descriptors_preserve_exact_stream_params() {
        let spec = StreamSpec {
            rate_hz: 44_100,
            layout: ChannelLayout::new(2, 0x3),
            encoding: SampleEncoding::F32,
        };
        // Rate/channels are descriptor inputs, not something candidates alter;
        // each candidate must be expressible without touching them.
        let c = &candidates_for(spec.encoding)[0];
        assert_eq!(
            c.container_bits as usize,
            c.format.bytes_per_sample() * 8
        );
    }
}
