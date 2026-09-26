//! DSD output mode selection: pure candidate math, platform-independent.
//!
//! Candidate order is a fixed preference: widest native DSD slot first
//! (fewest ALSA frames per second), then DoP as a PCM-wire fallback.
//! Rate math: a slot of N bits carries N DSD bits per frame, so the ALSA
//! rate parameter is `dsd_rate_hz / N` for native modes. DoP carries 16 DSD
//! bits per 24-bit word, so the wire rate is `dsd_rate_hz / 16`.
//!
//! Compiled on every platform (unit-tested on Windows); only the Linux
//! `imp` module talks to ALSA.

use sointty_core::{DeviceFormat, PlayerError, SampleEncoding};

/// Wire-level sample container a candidate negotiates with ALSA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireFormat {
    /// `SND_PCM_FORMAT_DSD_U32_LE`.
    DsdU32Le,
    /// `SND_PCM_FORMAT_DSD_U16_LE`.
    DsdU16Le,
    /// `SND_PCM_FORMAT_DSD_U8`.
    DsdU8,
    /// `SND_PCM_FORMAT_S24_3LE` — the DoP carrier.
    S24_3Le,
}

impl WireFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DsdU32Le => "DSD_U32_LE",
            Self::DsdU16Le => "DSD_U16_LE",
            Self::DsdU8 => "DSD_U8",
            Self::S24_3Le => "S24_3LE (DoP)",
        }
    }
}

/// One exact hardware mode to try, in preference order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DsdCandidate {
    /// ALSA rate parameter: `dsd_rate_hz / slot bits` (native) or
    /// `dsd_rate_hz / 16` (DoP wire rate).
    pub(crate) rate_hz: u32,
    pub(crate) device_format: DeviceFormat,
    pub(crate) valid_bits: u8,
    pub(crate) wire: WireFormat,
}

/// Candidate DSD hardware modes for a DSD bit rate, in preference order:
/// native `DSD_U32_LE`, `DSD_U16_LE`, `DSD_U8`, then the DoP fallback.
/// Modes whose rate math does not divide evenly are dropped; a zero or odd
/// rate leaves no exact mode at all (empty result).
pub(crate) fn dsd_candidates(dsd_rate_hz: u32) -> Vec<DsdCandidate> {
    let mut out = Vec::new();
    if dsd_rate_hz == 0 {
        return out;
    }
    for (slot_bits, device_format, wire) in [
        (32_u32, DeviceFormat::DsdU32Le, WireFormat::DsdU32Le),
        (16, DeviceFormat::DsdU16Le, WireFormat::DsdU16Le),
        (8, DeviceFormat::DsdU8, WireFormat::DsdU8),
    ] {
        if dsd_rate_hz % slot_bits == 0 {
            out.push(DsdCandidate {
                rate_hz: dsd_rate_hz / slot_bits,
                device_format,
                valid_bits: SampleEncoding::Dsd.valid_bits(),
                wire,
            });
        }
    }
    if dsd_rate_hz % 16 == 0 {
        out.push(DsdCandidate {
            rate_hz: dsd_rate_hz / 16,
            device_format: DeviceFormat::Dop24,
            valid_bits: 24,
            wire: WireFormat::S24_3Le,
        });
    }
    out
}

/// Typed rejection when no candidate exists or the device accepted none.
pub(crate) fn no_dsd_mode_error(dsd_rate_hz: u32, channels: u16) -> PlayerError {
    PlayerError::UnsupportedFormat {
        rate_hz: dsd_rate_hz,
        channels,
        encoding: SampleEncoding::Dsd,
        reason: "no native DSD or DoP mode accepted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsd64_candidates_preference_rates_and_mapping() {
        let c = dsd_candidates(2_822_400);
        assert_eq!(c.len(), 4);

        assert_eq!(c[0].device_format, DeviceFormat::DsdU32Le);
        assert_eq!(c[0].rate_hz, 88_200);
        assert_eq!(c[0].wire, WireFormat::DsdU32Le);
        assert_eq!(c[0].wire.as_str(), "DSD_U32_LE");

        assert_eq!(c[1].device_format, DeviceFormat::DsdU16Le);
        assert_eq!(c[1].rate_hz, 176_400);
        assert_eq!(c[1].wire, WireFormat::DsdU16Le);
        assert_eq!(c[1].wire.as_str(), "DSD_U16_LE");

        assert_eq!(c[2].device_format, DeviceFormat::DsdU8);
        assert_eq!(c[2].rate_hz, 352_800);
        assert_eq!(c[2].wire, WireFormat::DsdU8);
        assert_eq!(c[2].wire.as_str(), "DSD_U8");

        assert_eq!(c[3].device_format, DeviceFormat::Dop24);
        assert_eq!(c[3].rate_hz, 176_400);
        assert_eq!(c[3].wire, WireFormat::S24_3Le);
        assert_eq!(c[3].wire.as_str(), "S24_3LE (DoP)");
        assert_eq!(c[3].valid_bits, 24);

        // Native modes report the DSD encoding's valid bits.
        for native in &c[..3] {
            assert_eq!(native.valid_bits, SampleEncoding::Dsd.valid_bits());
            assert!(native.device_format.is_dsd());
        }
        assert!(c[3].device_format.is_dsd());
    }

    #[test]
    fn dsd128_candidates() {
        let c = dsd_candidates(5_644_800);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].rate_hz, 176_400);
        assert_eq!(c[0].device_format, DeviceFormat::DsdU32Le);
        assert_eq!(c[1].rate_hz, 352_800);
        assert_eq!(c[1].device_format, DeviceFormat::DsdU16Le);
        assert_eq!(c[2].rate_hz, 705_600);
        assert_eq!(c[2].device_format, DeviceFormat::DsdU8);
        assert_eq!(c[3].rate_hz, 352_800);
        assert_eq!(c[3].device_format, DeviceFormat::Dop24);
        // Preference order is unchanged: U32, U16, U8, DoP.
        let order: Vec<_> = c.iter().map(|c| c.device_format).collect();
        assert_eq!(
            order,
            [
                DeviceFormat::DsdU32Le,
                DeviceFormat::DsdU16Le,
                DeviceFormat::DsdU8,
                DeviceFormat::Dop24,
            ]
        );
    }

    #[test]
    fn non_divisible_rates_drop_only_those_candidates() {
        // 176_400 = 16 * 11_025 = 8 * 22_050, but not a multiple of 32:
        // the U32 candidate is dropped, the rest remain in order.
        let c = dsd_candidates(176_400);
        let formats: Vec<_> = c.iter().map(|c| c.device_format).collect();
        assert_eq!(
            formats,
            [
                DeviceFormat::DsdU16Le,
                DeviceFormat::DsdU8,
                DeviceFormat::Dop24,
            ]
        );
        assert_eq!(c[0].rate_hz, 11_025);
        assert_eq!(c[1].rate_hz, 22_050);
        assert_eq!(c[2].rate_hz, 11_025);

        // Odd rate: nothing divides evenly.
        assert!(dsd_candidates(2_822_401).is_empty());
        // Zero rate: no candidates either.
        assert!(dsd_candidates(0).is_empty());
    }

    #[test]
    fn rejection_structure_when_no_candidates_exist() {
        assert!(dsd_candidates(2_822_401).is_empty());
        let err = no_dsd_mode_error(2_822_401, 2);
        match err {
            PlayerError::UnsupportedFormat {
                rate_hz,
                channels,
                encoding,
                reason,
            } => {
                assert_eq!(rate_hz, 2_822_401);
                assert_eq!(channels, 2);
                assert_eq!(encoding, SampleEncoding::Dsd);
                assert_eq!(reason, "no native DSD or DoP mode accepted");
            }
            other => panic!("expected UnsupportedFormat, got {other:?}"),
        }
    }
}
