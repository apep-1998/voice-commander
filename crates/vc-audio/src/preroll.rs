//! The pre-roll buffer: a rolling window of the most recent audio.
//!
//! This is what makes a recording able to start *before* the keypress. The cost is a fixed
//! allocation held for as long as the device is open; the benefit is that the first syllable
//! of "open my calendar" survives, which every push-to-talk tool without one loses.
//!
//! Nothing here is ever written to disk unless a recording is actually triggered.

/// A fixed-capacity ring holding the most recently captured samples.
///
/// Writes are O(n) in the samples written and never allocate after construction, which
/// matters because this is fed continuously for as long as the microphone is open.
#[derive(Debug)]
pub struct PreRollBuffer {
    samples: Box<[f32]>,
    /// Where the next sample goes.
    head: usize,
    /// How many slots hold real audio; stops growing once the ring has wrapped.
    filled: usize,
    sample_rate: u32,
}

impl PreRollBuffer {
    /// A buffer holding at most `window_ms` of mono audio at `sample_rate`.
    ///
    /// A zero-length window is legal and yields a buffer that accepts writes and returns
    /// nothing — which is precisely `CaptureMode::Warm`, so that mode needs no special case
    /// anywhere else.
    pub fn new(sample_rate: u32, window_ms: u32) -> Self {
        let capacity = samples_for_ms(sample_rate, window_ms);
        Self {
            samples: vec![0.0; capacity].into_boxed_slice(),
            head: 0,
            filled: 0,
            sample_rate,
        }
    }

    pub fn capacity(&self) -> usize {
        self.samples.len()
    }

    /// How much audio is currently available, in milliseconds.
    ///
    /// Less than the configured window until the ring has filled, which is why a recording
    /// started moments after the device opened reports a smaller pre-roll than configured.
    pub fn available_ms(&self) -> u32 {
        if self.sample_rate == 0 {
            return 0;
        }
        u32::try_from(self.filled as u64 * 1000 / u64::from(self.sample_rate)).unwrap_or(u32::MAX)
    }

    pub fn available_samples(&self) -> usize {
        self.filled
    }

    /// Append samples, discarding whatever falls out of the window.
    pub fn push(&mut self, samples: &[f32]) {
        let capacity = self.samples.len();
        if capacity == 0 {
            return;
        }

        // Writing more than the whole window at once is not worth a wrapping loop: only the
        // tail can survive, so copy just that.
        let incoming = if samples.len() > capacity {
            &samples[samples.len() - capacity..]
        } else {
            samples
        };

        let first = (capacity - self.head).min(incoming.len());
        self.samples[self.head..self.head + first].copy_from_slice(&incoming[..first]);
        let rest = incoming.len() - first;
        if rest > 0 {
            self.samples[..rest].copy_from_slice(&incoming[first..]);
        }

        self.head = (self.head + incoming.len()) % capacity;
        self.filled = (self.filled + incoming.len()).min(capacity);
    }

    /// The most recent `want_ms` of audio, oldest sample first.
    ///
    /// Returns everything available when less than that has been captured.
    pub fn take_last_ms(&self, want_ms: u32) -> Vec<f32> {
        let wanted = samples_for_ms(self.sample_rate, want_ms).min(self.filled);
        let capacity = self.samples.len();
        if wanted == 0 || capacity == 0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(wanted);
        // `head` is one past the newest sample, so the oldest wanted sample sits `wanted`
        // slots behind it — modular, because it may be on the other side of the wrap.
        let start = (self.head + capacity - wanted) % capacity;
        let first = (capacity - start).min(wanted);
        out.extend_from_slice(&self.samples[start..start + first]);
        if first < wanted {
            out.extend_from_slice(&self.samples[..wanted - first]);
        }
        out
    }

    /// Forget everything captured so far, keeping the allocation.
    ///
    /// Used when the device is reopened: audio from before a device change is not lookback,
    /// it is a splice of two different microphones.
    pub fn clear(&mut self) {
        self.head = 0;
        self.filled = 0;
    }
}

fn samples_for_ms(sample_rate: u32, ms: u32) -> usize {
    usize::try_from(u64::from(sample_rate) * u64::from(ms) / 1000).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ramp, so every sample is identifiable and an off-by-one is visible.
    fn ramp(from: usize, count: usize) -> Vec<f32> {
        (from..from + count).map(|n| n as f32).collect()
    }

    #[test]
    fn an_unfilled_buffer_reports_only_what_it_has() {
        let mut buffer = PreRollBuffer::new(1000, 500); // 500 samples
        buffer.push(&ramp(0, 100));

        assert_eq!(buffer.available_samples(), 100);
        assert_eq!(buffer.available_ms(), 100);
        // Asking for more than exists yields what exists, rather than silence padding.
        assert_eq!(buffer.take_last_ms(500), ramp(0, 100));
    }

    #[test]
    fn the_oldest_audio_falls_out_of_the_window() {
        let mut buffer = PreRollBuffer::new(1000, 100); // 100 samples
        buffer.push(&ramp(0, 150));

        assert_eq!(buffer.available_samples(), 100);
        assert_eq!(buffer.take_last_ms(100), ramp(50, 100));
    }

    #[test]
    fn samples_stay_in_order_across_the_wrap() {
        // The bug this catches — reading the ring without accounting for the wrap — produces
        // audio that is subtly scrambled rather than obviously broken.
        let mut buffer = PreRollBuffer::new(1000, 100);
        buffer.push(&ramp(0, 60));
        buffer.push(&ramp(60, 60)); // crosses the end of the ring

        assert_eq!(buffer.take_last_ms(100), ramp(20, 100));
    }

    #[test]
    fn many_small_writes_match_one_large_one() {
        let mut small = PreRollBuffer::new(1000, 100);
        for n in 0..250 {
            small.push(&[n as f32]);
        }
        let mut large = PreRollBuffer::new(1000, 100);
        large.push(&ramp(0, 250));

        // A real backend delivers arbitrary buffer sizes, so these must be indistinguishable.
        assert_eq!(small.take_last_ms(100), large.take_last_ms(100));
    }

    #[test]
    fn a_write_larger_than_the_window_keeps_only_its_tail() {
        let mut buffer = PreRollBuffer::new(1000, 100);
        buffer.push(&ramp(0, 1000));
        assert_eq!(buffer.take_last_ms(100), ramp(900, 100));
    }

    #[test]
    fn a_partial_request_takes_from_the_newest_end() {
        // Pre-roll means "the audio just before the keypress", not "the start of the buffer".
        let mut buffer = PreRollBuffer::new(1000, 100);
        buffer.push(&ramp(0, 100));
        assert_eq!(buffer.take_last_ms(30), ramp(70, 30));
    }

    #[test]
    fn a_zero_length_window_is_valid_and_yields_nothing() {
        // This is `CaptureMode::Warm`: the device stays open, but there is no lookback. It
        // needs no special case anywhere else because the buffer handles it.
        let mut buffer = PreRollBuffer::new(16_000, 0);
        buffer.push(&ramp(0, 1000));

        assert_eq!(buffer.capacity(), 0);
        assert_eq!(buffer.available_ms(), 0);
        assert!(buffer.take_last_ms(500).is_empty());
    }

    #[test]
    fn clearing_discards_audio_from_before_a_device_change() {
        // Splicing two different microphones together is worse than having no lookback.
        let mut buffer = PreRollBuffer::new(1000, 100);
        buffer.push(&ramp(0, 100));
        buffer.clear();

        assert_eq!(buffer.available_samples(), 0);
        assert!(buffer.take_last_ms(100).is_empty());

        buffer.push(&ramp(500, 10));
        assert_eq!(buffer.take_last_ms(100), ramp(500, 10));
    }

    #[test]
    fn the_window_is_sized_from_the_sample_rate() {
        assert_eq!(PreRollBuffer::new(16_000, 500).capacity(), 8_000);
        assert_eq!(PreRollBuffer::new(48_000, 500).capacity(), 24_000);
        assert_eq!(PreRollBuffer::new(16_000, 1_000).capacity(), 16_000);
    }
}
