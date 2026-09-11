//! The boundary between "some backend produces samples" and everything else.
//!
//! Capture backends differ wildly — cpal, a native PipeWire client, a file replaying a
//! fixture — but they all do one thing: hand blocks of samples to something else, from a
//! thread whose deadlines must not be missed. That is the whole of this interface.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use vc_core::config::DeviceSelector;

/// Why capture failed.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// No input device matched, or there are none at all.
    #[error("no capture device available{}", match .requested {
        Some(name) => format!(" matching {name:?}"),
        None => String::new(),
    })]
    NoDevice { requested: Option<String> },
    /// A device exists but will not produce the format asked for.
    #[error("{device} does not support {sample_rate} Hz with {channels} channel(s)")]
    UnsupportedFormat {
        device: String,
        sample_rate: u32,
        channels: u16,
    },
    /// The backend failed after the stream was running.
    #[error("capture stream failed: {0}")]
    Stream(String),
    #[error("audio backend: {0}")]
    Backend(String),
}

/// What a backend is about to produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// The realtime-safe end of the pipeline, handed to a backend.
///
/// [`SampleSink::write`] never allocates, never locks and never blocks. That is not a
/// nicety: it is called from a thread with a hard deadline, and anything that stalls it
/// produces a gap in the user's recording. When the consumer cannot keep up, samples are
/// dropped and counted — a short gap is recoverable, a stalled audio thread is not.
#[derive(Debug)]
pub struct SampleSink {
    producer: rtrb::Producer<f32>,
    dropped: Arc<AtomicU64>,
}

impl SampleSink {
    /// Write a block, dropping whatever does not fit.
    pub fn write(&mut self, samples: &[f32]) {
        let mut dropped = 0u64;
        for &sample in samples {
            if self.producer.push(sample).is_err() {
                dropped += 1;
            }
        }
        if dropped > 0 {
            // A relaxed increment: the exact interleaving does not matter, only that the
            // total is eventually visible to the thread that reports xruns.
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
    }
}

/// The ordinary-thread end, where all the real work happens.
#[derive(Debug)]
pub struct SampleSource {
    consumer: rtrb::Consumer<f32>,
    dropped: Arc<AtomicU64>,
}

impl SampleSource {
    /// Move everything currently queued into `out`, returning how many samples arrived.
    pub fn drain(&mut self, out: &mut Vec<f32>) -> usize {
        let mut count = 0;
        while let Ok(sample) = self.consumer.pop() {
            out.push(sample);
            count += 1;
        }
        count
    }

    /// Samples lost because this side could not keep up. Monotonic for the channel's life.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.consumer.is_empty()
    }
}

/// Create a capture channel with room for `capacity` samples.
///
/// Sized in the caller from the sample rate: it needs to absorb the consumer being
/// descheduled, without being so large that a genuinely stuck consumer hides behind it.
pub fn channel(capacity: usize) -> (SampleSink, SampleSource) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        SampleSink {
            producer,
            dropped: Arc::clone(&dropped),
        },
        SampleSource { consumer, dropped },
    )
}

/// Something that produces captured audio.
pub trait AudioSource: Send {
    /// What this will produce, once started.
    fn info(&self) -> DeviceInfo;

    /// Begin delivering mono samples into `sink`.
    ///
    /// Backends that produce interleaved multi-channel audio downmix before writing, so that
    /// everything downstream deals in mono only.
    fn start(&mut self, sink: SampleSink) -> Result<(), AudioError>;

    /// Stop delivering and release the device.
    fn stop(&mut self);
}

/// Enumerate what a backend can capture from.
pub trait AudioHost {
    fn devices(&self) -> Result<Vec<DeviceInfo>, AudioError>;

    /// Resolve a configured selector to a concrete device name.
    fn resolve(&self, selector: &DeviceSelector) -> Result<DeviceInfo, AudioError>;
}

/// A backend that replays samples held in memory.
///
/// This is what makes the rest of the crate — and the session machinery built on it —
/// testable without sound hardware. Real time is optional: a test asserting on pre-roll
/// boundaries wants to control exactly how much audio has been delivered, and a test
/// asserting on timing wants the clock.
pub struct SyntheticSource {
    info: DeviceInfo,
    samples: Vec<f32>,
    /// Samples per delivered block, mirroring a real backend's buffer size.
    block: usize,
    /// Deliver in real time rather than as fast as possible.
    realtime: bool,
    /// Start again from the beginning instead of ending — a microphone does not run out.
    looping: bool,
    thread: Option<std::thread::JoinHandle<()>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for SyntheticSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntheticSource")
            .field("info", &self.info)
            .field("samples", &self.samples.len())
            .field("realtime", &self.realtime)
            .finish_non_exhaustive()
    }
}

impl SyntheticSource {
    pub fn new(sample_rate: u32, samples: Vec<f32>) -> Self {
        Self {
            info: DeviceInfo {
                name: "synthetic".to_owned(),
                sample_rate,
                channels: 1,
            },
            samples,
            block: 480,
            realtime: false,
            looping: false,
            thread: None,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Deliver in blocks of `samples`, as a real backend's buffer size would.
    pub fn with_block(mut self, samples: usize) -> Self {
        self.block = samples.max(1);
        self
    }

    /// Pace delivery against the clock instead of running flat out.
    pub fn realtime(mut self) -> Self {
        self.realtime = true;
        self
    }

    /// Repeat forever, as an idle microphone effectively does.
    pub fn looping(mut self) -> Self {
        self.looping = true;
        self
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.info.name = name.into();
        self
    }
}

impl AudioSource for SyntheticSource {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn start(&mut self, mut sink: SampleSink) -> Result<(), AudioError> {
        let samples = self.samples.clone();
        let block = self.block;
        let realtime = self.realtime;
        let looping = self.looping;
        let rate = self.info.sample_rate;
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Relaxed);

        self.thread = Some(std::thread::spawn(move || loop {
            for chunk in samples.chunks(block) {
                if !running.load(Ordering::Relaxed) {
                    return;
                }
                sink.write(chunk);
                if realtime && rate > 0 {
                    std::thread::sleep(std::time::Duration::from_micros(
                        chunk.len() as u64 * 1_000_000 / u64::from(rate),
                    ));
                }
            }
            if !looping || samples.is_empty() {
                return;
            }
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_travel_from_a_backend_to_the_consumer_in_order() {
        let (sink, mut source) = channel(1024);
        let mut backend = SyntheticSource::new(16_000, (0..500).map(|n| n as f32).collect());
        backend.start(sink).expect("starts");

        let mut out = Vec::new();
        for _ in 0..1_000 {
            source.drain(&mut out);
            if out.len() >= 500 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        backend.stop();

        assert_eq!(out.len(), 500);
        assert_eq!(out, (0..500).map(|n| n as f32).collect::<Vec<_>>());
    }

    #[test]
    fn a_full_channel_drops_samples_and_counts_them_instead_of_blocking() {
        // The alternative is stalling the audio thread, which turns a recoverable gap into a
        // recording that stops dead.
        let (mut sink, source) = channel(16);
        sink.write(&vec![1.0; 100]);

        assert_eq!(source.dropped(), 84);
    }

    #[test]
    fn nothing_is_dropped_while_the_consumer_keeps_up() {
        let (mut sink, mut source) = channel(64);
        let mut out = Vec::new();
        for _ in 0..100 {
            sink.write(&[0.5; 32]);
            source.drain(&mut out);
        }

        assert_eq!(source.dropped(), 0);
        assert_eq!(out.len(), 3_200);
    }

    #[test]
    fn a_backend_can_be_stopped_while_it_is_still_producing() {
        let mut backend = SyntheticSource::new(16_000, vec![0.1; 48_000])
            .with_block(64)
            .looping();
        let (sink, _source) = channel(128);
        backend.start(sink).expect("starts");
        // Stopping must return rather than waiting for a source that never ends.
        backend.stop();
    }

    #[test]
    fn block_size_does_not_change_what_arrives() {
        // A real backend picks its own buffer size, and may change it.
        let expected: Vec<f32> = (0..1_000).map(|n| n as f32).collect();

        for block in [1, 7, 64, 480, 2_048] {
            let (sink, mut source) = channel(4_096);
            let mut backend = SyntheticSource::new(16_000, expected.clone()).with_block(block);
            backend.start(sink).expect("starts");

            let mut out = Vec::new();
            for _ in 0..1_000 {
                source.drain(&mut out);
                if out.len() >= expected.len() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            backend.stop();

            assert_eq!(out, expected, "block size {block} changed the output");
        }
    }

    #[test]
    fn device_errors_say_which_device_and_which_format() {
        let error = AudioError::NoDevice {
            requested: Some("Yeti".to_owned()),
        };
        assert!(error.to_string().contains("Yeti"), "{error}");

        let error = AudioError::UnsupportedFormat {
            device: "Built-in".to_owned(),
            sample_rate: 16_000,
            channels: 1,
        };
        assert!(error.to_string().contains("16000"), "{error}");
    }
}
