#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;
#[cfg(target_os = "linux")]
use std::thread::JoinHandle;

#[cfg(target_os = "linux")]
use sointty_core::{
    AudioOutput, BufferConfig, DsdSpec, OutputCounters, OutputSpec, PlayerError, SampleEncoding,
    StreamSpec, validate_device_selection, validate_exact_spec,
};
use sointty_core::{ChannelLayout, DeviceFormat, DeviceId};

#[cfg(not(target_os = "linux"))]
use sointty_core::PlayerError;

#[cfg(any(target_os = "linux", test))]
mod dsd;

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::dsd::{DsdCandidate, WireFormat, dsd_candidates, no_dsd_mode_error};
    use alsa::PCM;
    use alsa::pcm::{Access, Format, HwParams};
    use alsa::{Direction, ValueOr};

    pub struct AlsaOutput {
        device: DeviceId,
        pcm: Option<PCM>,
        spec: Option<OutputSpec>,
        thread: Option<JoinHandle<()>>,
    }

    impl AlsaOutput {
        pub fn new(device: DeviceId) -> Result<Self, PlayerError> {
            validate_device_selection(&device)?;
            Ok(Self {
                device,
                pcm: None,
                spec: None,
                thread: None,
            })
        }
    }

    impl AudioOutput for AlsaOutput {
        fn configure(
            &mut self,
            input: &StreamSpec,
            buffers: BufferConfig,
        ) -> Result<OutputSpec, PlayerError> {
            validate_exact_spec(input)?;
            self.stop()?;

            let pcm = PCM::new(&self.device, Direction::Playback, false).map_err(|error| {
                if error.errno() == 16 {
                    PlayerError::DeviceBusy
                } else {
                    PlayerError::Output
                }
            })?;

            let candidates: &[(DeviceFormat, Format)] = match input.encoding {
                SampleEncoding::S16 => &[(DeviceFormat::S16Le, Format::S16LE)],
                SampleEncoding::S24 => &[
                    (DeviceFormat::S24_3Le, Format::S243LE),
                    (DeviceFormat::S24In32Low, Format::S24LE),
                    (DeviceFormat::S24In32High, Format::S24LE),
                ],
                SampleEncoding::S32 => &[(DeviceFormat::S32Le, Format::S32LE)],
                SampleEncoding::F32 => &[(DeviceFormat::F32Le, Format::FloatLE)],
            };

            let mut configured = None;
            for (device_format, alsa_format) in candidates {
                let result =
                    configure_candidate(&pcm, input, buffers, *alsa_format, *device_format);
                match result {
                    Ok(spec) => {
                        configured = Some(spec);
                        break;
                    }
                    Err(PlayerError::UnsupportedFormat { .. } | PlayerError::Output) => continue,
                    Err(error) => return Err(error),
                }
            }
            let mut spec = configured.ok_or(PlayerError::UnsupportedFormat {
                rate_hz: input.rate_hz,
                channels: input.layout.channels,
                encoding: input.encoding,
                reason: "device rejected all exact-rate hardware-native PCM candidates",
            })?;
            spec.device = self.device.clone();
            self.spec = Some(spec.clone());
            self.pcm = Some(pcm);
            Ok(spec)
        }

        fn configure_dsd(
            &mut self,
            input: &DsdSpec,
            buffers: BufferConfig,
        ) -> Result<OutputSpec, PlayerError> {
            self.stop()?;

            let pcm = PCM::new(&self.device, Direction::Playback, false).map_err(|error| {
                if error.errno() == 16 {
                    PlayerError::DeviceBusy
                } else {
                    PlayerError::Output
                }
            })?;

            let candidates = dsd_candidates(input.dsd_rate_hz);
            let mut configured = None;
            for candidate in &candidates {
                let alsa_format = match candidate.wire {
                    WireFormat::DsdU32Le => Format::DSDU32LE,
                    WireFormat::DsdU16Le => Format::DSDU16LE,
                    WireFormat::DsdU8 => Format::DSDU8,
                    WireFormat::S24_3Le => Format::S243LE,
                };
                let result = configure_dsd_candidate(&pcm, input, buffers, alsa_format, candidate);
                match result {
                    Ok(spec) => {
                        configured = Some((candidate, spec));
                        break;
                    }
                    Err(PlayerError::UnsupportedFormat { .. } | PlayerError::Output) => continue,
                    Err(error) => return Err(error),
                }
            }
            // Negotiated parameters are surfaced through the returned
            // `OutputSpec`; printing here would corrupt the TUI.
            let (_, mut spec) =
                configured.ok_or_else(|| no_dsd_mode_error(input.dsd_rate_hz, input.layout.channels))?;
            spec.device = self.device.clone();
            self.spec = Some(spec.clone());
            self.pcm = Some(pcm);
            Ok(spec)
        }

        fn start(
            &mut self,
            mut pcm: rtrb::Consumer<u8>,
            counters: Arc<OutputCounters>,
        ) -> Result<(), PlayerError> {
            let device = self.pcm.take().ok_or(PlayerError::Output)?;
            let spec = self.spec.clone().ok_or(PlayerError::Output)?;
            let frame_bytes = spec.bytes_per_frame();
            let handle = std::thread::Builder::new()
                .name("sointty-output".to_owned())
                .spawn(move || output_loop(device, spec, frame_bytes, &mut pcm, counters))
                .map_err(|_| PlayerError::Output)?;
            self.thread = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), PlayerError> {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            if let Some(pcm) = self.pcm.take() {
                let _ = pcm.drop();
            }
            self.spec = None;
            Ok(())
        }
    }

    impl Drop for AlsaOutput {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    fn configure_candidate(
        pcm: &PCM,
        input: &StreamSpec,
        buffers: BufferConfig,
        format: Format,
        device_format: DeviceFormat,
    ) -> Result<OutputSpec, PlayerError> {
        let hw = HwParams::any(pcm).map_err(|_| PlayerError::Output)?;
        hw.set_access(Access::RWInterleaved)
            .map_err(|_| unsupported(input, "interleaved output unavailable"))?;
        hw.set_format(format)
            .map_err(|_| unsupported(input, "device rejected exact PCM format"))?;
        hw.set_channels(u32::from(input.layout.channels))
            .map_err(|_| unsupported(input, "device rejected channel count"))?;
        hw.set_rate(input.rate_hz, ValueOr::Nearest)
            .map_err(|_| unsupported(input, "device rejected exact sample rate"))?;
        let actual_rate = hw.get_rate().map_err(|_| PlayerError::Output)?;
        if actual_rate != input.rate_hz {
            return Err(unsupported(
                input,
                "device would run at a different sample rate",
            ));
        }
        if hw.get_rate_resample().map_err(|_| PlayerError::Output)? {
            return Err(unsupported(input, "ALSA hardware resampling is enabled"));
        }
        hw.set_period_size(i64::from(buffers.period_frames), ValueOr::Nearest)
            .map_err(|_| PlayerError::Output)?;
        hw.set_buffer_size(i64::from(buffers.buffer_frames))
            .map_err(|_| PlayerError::Output)?;
        pcm.hw_params(&hw).map_err(|_| PlayerError::Output)?;

        let sw = pcm.sw_params_current().map_err(|_| PlayerError::Output)?;
        sw.set_start_threshold(i64::from(buffers.period_frames) * 2)
            .map_err(|_| PlayerError::Output)?;
        sw.set_avail_min(i64::from(buffers.period_frames))
            .map_err(|_| PlayerError::Output)?;
        sw.set_stop_threshold(i64::from(buffers.buffer_frames))
            .map_err(|_| PlayerError::Output)?;
        pcm.sw_params(&sw).map_err(|_| PlayerError::Output)?;

        Ok(OutputSpec {
            device: String::new(),
            rate_hz: input.rate_hz,
            layout: input.layout,
            format: device_format,
            valid_bits: input.encoding.valid_bits(),
        })
    }

    /// Try one DSD candidate with exact hardware parameters. The DoP leg is
    /// no different at this layer: it requires the endpoint to accept the
    /// 24-bit 176.4 kHz PCM descriptor exactly.
    fn configure_dsd_candidate(
        pcm: &PCM,
        input: &DsdSpec,
        buffers: BufferConfig,
        alsa_format: Format,
        candidate: &DsdCandidate,
    ) -> Result<OutputSpec, PlayerError> {
        let reject = |reason: &'static str| PlayerError::UnsupportedFormat {
            rate_hz: input.dsd_rate_hz,
            channels: input.layout.channels,
            encoding: SampleEncoding::Dsd,
            reason,
        };
        let hw = HwParams::any(pcm).map_err(|_| PlayerError::Output)?;
        hw.set_access(Access::RWInterleaved)
            .map_err(|_| reject("interleaved output unavailable"))?;
        hw.set_format(alsa_format)
            .map_err(|_| reject("device rejected exact DSD/DoP wire format"))?;
        hw.set_channels(u32::from(input.layout.channels))
            .map_err(|_| reject("device rejected channel count"))?;
        hw.set_rate(candidate.rate_hz, ValueOr::Nearest)
            .map_err(|_| reject("device rejected exact DSD rate"))?;
        let actual_rate = hw.get_rate().map_err(|_| PlayerError::Output)?;
        if actual_rate != candidate.rate_hz {
            return Err(reject("device would run at a different sample rate"));
        }
        if hw.get_rate_resample().map_err(|_| PlayerError::Output)? {
            return Err(reject("ALSA hardware resampling is enabled"));
        }
        hw.set_period_size(i64::from(buffers.period_frames), ValueOr::Nearest)
            .map_err(|_| PlayerError::Output)?;
        hw.set_buffer_size(i64::from(buffers.buffer_frames))
            .map_err(|_| PlayerError::Output)?;
        pcm.hw_params(&hw).map_err(|_| PlayerError::Output)?;

        let sw = pcm.sw_params_current().map_err(|_| PlayerError::Output)?;
        sw.set_start_threshold(i64::from(buffers.period_frames) * 2)
            .map_err(|_| PlayerError::Output)?;
        sw.set_avail_min(i64::from(buffers.period_frames))
            .map_err(|_| PlayerError::Output)?;
        sw.set_stop_threshold(i64::from(buffers.buffer_frames))
            .map_err(|_| PlayerError::Output)?;
        pcm.sw_params(&sw).map_err(|_| PlayerError::Output)?;

        Ok(OutputSpec {
            device: String::new(),
            rate_hz: candidate.rate_hz,
            layout: input.layout,
            format: candidate.device_format,
            valid_bits: candidate.valid_bits,
        })
    }

    fn output_loop(
        pcm: PCM,
        spec: OutputSpec,
        frame_bytes: usize,
        consumer: &mut rtrb::Consumer<u8>,
        counters: Arc<OutputCounters>,
    ) {
        let mut scratch = vec![0_u8; frame_bytes * 4096];
        let counter_scale = spec.format.source_frames_per_wire_frame();
        let io = pcm.io_bytes();
        if pcm.prepare().is_err() {
            counters.fault.store(true, Ordering::Relaxed);
            return;
        }

        loop {
            let available = consumer.slots();
            if available == 0 {
                if consumer.is_abandoned() {
                    return;
                }
                // The decoder may still be filling the initial buffer; an empty ring here is not
                // yet a hardware underrun. ALSA starts automatically at the configured threshold.
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            let bytes = available.min(scratch.len()) - (available.min(scratch.len()) % frame_bytes);
            if bytes == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            let (popped, _) = consumer.pop_partial_slice(&mut scratch[..bytes]);
            let offset = popped.len();
            let frames = offset / frame_bytes;
            let mut written_frames = 0_usize;
            while written_frames < frames {
                match io.writei(&scratch[written_frames * frame_bytes..frames * frame_bytes]) {
                    Ok(written) => {
                        written_frames += written as usize;
                        counters
                            .played_frames
                            .fetch_add(written as u64 * counter_scale, Ordering::Relaxed);
                    }
                    Err(error) if error.errno() == 32 => {
                        counters.xruns.fetch_add(1, Ordering::Relaxed);
                        if pcm.prepare().is_err() {
                            counters.fault.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                    Err(_) => {
                        counters.fault.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    }

    fn unsupported(input: &StreamSpec, reason: &'static str) -> PlayerError {
        PlayerError::UnsupportedFormat {
            rate_hz: input.rate_hz,
            channels: input.layout.channels,
            encoding: input.encoding,
            reason,
        }
    }
}

#[cfg(target_os = "linux")]
pub use imp::AlsaOutput;

#[cfg(not(target_os = "linux"))]
pub struct AlsaOutput {
    _private: (),
}

#[cfg(not(target_os = "linux"))]
impl AlsaOutput {
    pub fn new(_device: DeviceId) -> Result<Self, PlayerError> {
        Err(PlayerError::InvalidInput(
            "ALSA output is only available on Linux",
        ))
    }
}

pub fn supported_formats(_layout: ChannelLayout) -> &'static [DeviceFormat] {
    &[
        DeviceFormat::S16Le,
        DeviceFormat::S24_3Le,
        DeviceFormat::S24In32Low,
        DeviceFormat::S24In32High,
        DeviceFormat::S32Le,
        DeviceFormat::F32Le,
    ]
}
