//! The real capture backend, via cpal.
//!
//! On a PipeWire desktop this reaches the sound server through its ALSA compatibility layer,
//! which is the well-trodden path and costs far less code than a native PipeWire client. The
//! [`AudioSource`](crate::AudioSource) trait means swapping in a native backend later is an
//! addition rather than a rewrite.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::FromSample;
use cpal::{Device, SampleFormat, StreamConfig, SupportedStreamConfigRange};
use tracing::{debug, warn};
use vc_core::config::DeviceSelector;

use crate::resample::to_mono;
use crate::source::{AudioError, AudioHost, AudioSource, DeviceInfo, SampleSink};

/// Enumerates and opens real input devices.
#[derive(Debug, Default)]
pub struct CpalHost;

impl CpalHost {
    pub fn new() -> Self {
        Self
    }

    /// Open a capture stream, preferring `sample_rate` but accepting what the device offers.
    ///
    /// Asking the device for the target rate directly is the reason the resampler is
    /// normally idle: PipeWire will happily convert 48 kHz hardware to a 16 kHz stream, and
    /// its converter is better than anything worth writing here.
    pub fn open(
        &self,
        selector: &DeviceSelector,
        sample_rate: u32,
    ) -> Result<CpalSource, AudioError> {
        let (device, name) = self.find(selector)?;
        let (config, format) = choose_config(&device, sample_rate, &name)?;

        Ok(CpalSource {
            info: DeviceInfo {
                name,
                sample_rate: config.sample_rate.0,
                channels: config.channels,
            },
            device,
            config,
            format,
            stream: None,
            failed: Arc::new(AtomicBool::new(false)),
        })
    }

    fn find(&self, selector: &DeviceSelector) -> Result<(Device, String), AudioError> {
        let host = cpal::default_host();

        match selector {
            DeviceSelector::Default => {
                let device = host
                    .default_input_device()
                    .ok_or(AudioError::NoDevice { requested: None })?;
                let name = device.name().unwrap_or_else(|_| "default".to_owned());
                Ok((device, name))
            }
            DeviceSelector::Match(needle) => {
                let devices = host
                    .input_devices()
                    .map_err(|error| AudioError::Backend(error.to_string()))?;
                for device in devices {
                    let name = device.name().unwrap_or_default();
                    if selector.matches(&name) {
                        return Ok((device, name));
                    }
                }
                Err(AudioError::NoDevice {
                    requested: Some(needle.clone()),
                })
            }
        }
    }
}

impl AudioHost for CpalHost {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        let host = cpal::default_host();
        let devices = host
            .input_devices()
            .map_err(|error| AudioError::Backend(error.to_string()))?;

        Ok(devices
            .filter_map(|device| {
                let name = device.name().ok()?;
                let config = device.default_input_config().ok()?;
                Some(DeviceInfo {
                    name,
                    sample_rate: config.sample_rate().0,
                    channels: config.channels(),
                })
            })
            .collect())
    }

    fn resolve(&self, selector: &DeviceSelector) -> Result<DeviceInfo, AudioError> {
        let (device, name) = self.find(selector)?;
        let config = device
            .default_input_config()
            .map_err(|error| AudioError::Backend(error.to_string()))?;
        Ok(DeviceInfo {
            name,
            sample_rate: config.sample_rate().0,
            channels: config.channels(),
        })
    }
}

/// How much we would rather have one sample format than another.
///
/// Not cosmetic. This machine's wireless microphone advertises `U8` first, and taking the
/// first configuration offered meant capturing speech at 8 bits — audible quantisation noise
/// — from a device that also offers 32-bit float. Every format is handled either way; this
/// decides which one to ask for.
fn channel_score(channels: u16) -> u8 {
    match channels {
        // Speech is mono, and asking for mono means the sound server does the downmix.
        1 => 3,
        2 => 2,
        // ALSA's `default` PCM advertises a 32-channel configuration on this machine. Opened
        // as-is it produces silence, and even when it does not, 32 channels of a 1-channel
        // microphone is 31 channels of nothing dragging the downmixed average to zero.
        _ => 0,
    }
}

fn format_score(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 | SampleFormat::F64 => 5,
        SampleFormat::I32 | SampleFormat::U32 => 4,
        SampleFormat::I16 | SampleFormat::U16 => 3,
        SampleFormat::I8 | SampleFormat::U8 => 1,
        _ => 0,
    }
}

/// Pick a stream configuration, preferring the requested rate and the best format offered.
///
/// Preference order is deliberate. The requested rate first, because getting it avoids
/// resampling entirely. Then the widest sample format, because a device that offers both
/// `U8` and `F32` will happily hand over 8-bit audio if asked. Then the device's default,
/// which is the rate the hardware actually runs at.
fn choose_config(
    device: &Device,
    wanted_rate: u32,
    name: &str,
) -> Result<(StreamConfig, SampleFormat), AudioError> {
    let supported: Vec<SupportedStreamConfigRange> = device
        .supported_input_configs()
        .map_err(|error| AudioError::Backend(error.to_string()))?
        .collect();

    let best = supported
        .iter()
        .filter(|range| {
            range.min_sample_rate().0 <= wanted_rate && wanted_rate <= range.max_sample_rate().0
        })
        // Channel count first: a configuration with the right format but 32 channels is
        // worse than one with a modest format and one channel.
        .max_by_key(|range| {
            (
                channel_score(range.channels()),
                format_score(range.sample_format()),
            )
        })
        .filter(|range| channel_score(range.channels()) > 0);

    if let Some(range) = best {
        let config = range.with_sample_rate(cpal::SampleRate(wanted_rate));
        debug!(
            device = name,
            rate = wanted_rate,
            format = ?config.sample_format(),
            "opening at the requested rate"
        );
        return Ok((config.config(), config.sample_format()));
    }

    let fallback = device
        .default_input_config()
        .map_err(|_| AudioError::UnsupportedFormat {
            device: name.to_owned(),
            sample_rate: wanted_rate,
            channels: 1,
        })?;
    warn!(
        device = name,
        wanted = wanted_rate,
        got = fallback.sample_rate().0,
        "device will not provide the requested rate; audio will be resampled"
    );
    Ok((fallback.config(), fallback.sample_format()))
}

/// A running capture stream.
pub struct CpalSource {
    info: DeviceInfo,
    device: Device,
    config: StreamConfig,
    format: SampleFormat,
    stream: Option<cpal::Stream>,
    /// Set from the error callback, since a stream can die long after it started.
    failed: Arc<AtomicBool>,
}

impl std::fmt::Debug for CpalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalSource")
            .field("info", &self.info)
            .field("running", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

impl CpalSource {
    /// Whether the stream has died since it started.
    ///
    /// cpal reports this asynchronously — unplugging a USB microphone does not make the next
    /// call fail, it just stops delivering — so this has to be polled.
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

impl AudioSource for CpalSource {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn start(&mut self, sink: SampleSink) -> Result<(), AudioError> {
        let channels = self.config.channels;
        let failed = Arc::clone(&self.failed);

        // The callback owns the sink. Everything it does is bounded and allocation-free
        // except the mono downmix, which allocates only for genuinely multi-channel devices
        // — and never for the mono stream this asks for in the first place.
        let stream = build_stream(&self.device, &self.config, self.format, channels, sink, {
            move |error| {
                warn!(%error, "capture stream error");
                failed.store(true, Ordering::Relaxed);
            }
        })?;

        stream
            .play()
            .map_err(|error| AudioError::Stream(error.to_string()))?;
        self.stream = Some(stream);
        Ok(())
    }

    fn stop(&mut self) {
        // Dropping the stream is how cpal closes a device.
        self.stream = None;
    }
}

/// Build the stream for whichever sample format the device reports.
///
/// Devices are not shy about their variety — this machine alone offers `U8`, `I16` and
/// `F32` across its inputs — so every format cpal knows is handled, converted to `f32` once
/// here so that nothing downstream has to care.
fn build_stream(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    channels: u16,
    sink: SampleSink,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError> {
    macro_rules! build {
        ($sample:ty) => {
            build_typed::<$sample>(device, config, channels, sink, on_error)
        };
    }

    match format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        other => Err(AudioError::Backend(format!(
            "unsupported sample format {other:?}"
        ))),
    }
}

fn build_typed<T>(
    device: &Device,
    config: &StreamConfig,
    channels: u16,
    mut sink: SampleSink,
    on_error: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let floats: Vec<f32> = data.iter().map(|&s| f32::from_sample_(s)).collect();
                sink.write(&to_mono(&floats, channels));
            },
            on_error,
            None,
        )
        .map_err(|error| AudioError::Stream(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Everything below needs real sound hardware, which CI does not have. They are run by
    // hand with `cargo test -p vc-audio -- --ignored --nocapture`, and `voice-commander
    // mic-test` covers the same ground for users.

    #[test]
    #[ignore = "requires a capture device"]
    fn the_default_device_can_be_resolved() {
        let info = CpalHost::new()
            .resolve(&DeviceSelector::Default)
            .expect("a default input device");
        println!("default input: {info:?}");
        assert!(info.sample_rate > 0);
    }

    #[test]
    #[ignore = "requires a capture device"]
    fn devices_can_be_enumerated() {
        let devices = CpalHost::new().devices().expect("enumerate");
        for device in &devices {
            println!("{device:?}");
        }
        assert!(!devices.is_empty(), "no input devices found");
    }

    #[test]
    #[ignore = "requires a capture device"]
    fn audio_actually_arrives_from_the_default_device() {
        let mut source = CpalHost::new()
            .open(&DeviceSelector::Default, 16_000)
            .expect("open");
        println!("opened {:?}", source.info());

        let (sink, mut consumer) = crate::source::channel(16_000);
        source.start(sink).expect("start");
        std::thread::sleep(std::time::Duration::from_millis(500));

        let mut samples = Vec::new();
        consumer.drain(&mut samples);
        source.stop();

        assert!(
            !samples.is_empty(),
            "the device produced no audio in half a second"
        );
        assert!(!source.has_failed(), "the stream reported an error");
    }

    #[test]
    fn a_selector_that_matches_nothing_says_what_was_asked_for() {
        // This one needs no hardware: either there are no devices, or none match.
        let error = CpalHost::new()
            .resolve(&DeviceSelector::Match(
                "no-such-device-exists-anywhere".to_owned(),
            ))
            .expect_err("should not match");
        assert!(
            error.to_string().contains("no-such-device-exists-anywhere")
                || matches!(error, AudioError::Backend(_)),
            "{error}"
        );
    }
}
