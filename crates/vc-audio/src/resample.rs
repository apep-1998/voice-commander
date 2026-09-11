//! Rate conversion to the format recordings are stored in.
//!
//! Mostly unused in practice: the backend asks the device for the target rate directly, and
//! PipeWire obliges. This exists for when it cannot — a device with a fixed rate, or a
//! backend that refuses the request — and it exists as its own testable unit rather than as
//! a few lines buried in the capture path.

use rubato::{FftFixedIn, Resampler as _, ResamplerConstructionError};

/// Why rate conversion failed.
#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("cannot resample from {from} Hz to {to} Hz: {source}")]
    Unsupported {
        from: u32,
        to: u32,
        #[source]
        source: ResamplerConstructionError,
    },
    #[error("resampling failed: {0}")]
    Failed(#[from] rubato::ResampleError),
}

/// Converts mono audio from one rate to another.
///
/// Buffers internally, because the underlying algorithm works on fixed-size chunks while a
/// capture backend delivers whatever size it feels like.
///
/// Two corrections are applied that a naive wrapper misses, and both matter here more than
/// they would elsewhere:
///
/// * **The filter's output delay is discarded.** rubato reports a delay — 1040 output frames
///   for 44.1 kHz to 16 kHz — during which the output is the filter ramping up rather than
///   the audio. Left in, every recording would begin with a fraction of a second of nothing
///   and be shifted by that much. That is precisely the leading syllable the pre-roll buffer
///   exists to preserve, so losing it here would undo the whole feature.
/// * **The tail is drained by pushing silence.** The resampler holds back roughly a
///   sub-chunk internally; at 44.1 kHz that is 130ms permanently missing from the end of
///   every recording unless it is flushed out deliberately.
///
/// Together these make the output length exact: *n* input samples become
/// `n * to / from` output samples, not approximately that.
pub struct Resampler {
    inner: Option<FftFixedIn<f32>>,
    chunk: usize,
    pending: Vec<f32>,
    /// Output frames still to be discarded as the filter's start-up delay.
    skip: usize,
    /// Real output samples handed back so far.
    emitted: usize,
    /// Input samples accepted so far, which fixes the exact output length.
    consumed: usize,
    from: u32,
    to: u32,
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("pending", &self.pending.len())
            .field("emitted", &self.emitted)
            .finish()
    }
}

impl Resampler {
    /// Build a converter, or a pass-through when the rates already match.
    ///
    /// The matching case is the common one — asking the device for 16 kHz usually works — so
    /// it must cost nothing rather than running audio through a filter with no work to do.
    pub fn new(from: u32, to: u32) -> Result<Self, ResampleError> {
        if from == to {
            return Ok(Self {
                inner: None,
                chunk: 0,
                pending: Vec::new(),
                skip: 0,
                emitted: 0,
                consumed: 0,
                from,
                to,
            });
        }

        // A quarter-second chunk: long enough for the transform to be efficient, short
        // enough that the tail held back between calls stays small.
        let chunk = (from as usize / 4).max(256);
        let inner = FftFixedIn::<f32>::new(from as usize, to as usize, chunk, 2, 1)
            .map_err(|source| ResampleError::Unsupported { from, to, source })?;
        let skip = inner.output_delay();

        Ok(Self {
            inner: Some(inner),
            chunk,
            pending: Vec::new(),
            skip,
            emitted: 0,
            consumed: 0,
            from,
            to,
        })
    }

    pub fn is_pass_through(&self) -> bool {
        self.inner.is_none()
    }

    /// How many output samples `consumed` input samples should ultimately produce.
    fn expected_output(&self) -> usize {
        (self.consumed as u64 * u64::from(self.to) / u64::from(self.from)) as usize
    }

    /// Convert what can be converted, holding back the remainder.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<f32>, ResampleError> {
        if self.inner.is_none() {
            self.consumed += samples.len();
            self.emitted += samples.len();
            return Ok(samples.to_vec());
        }

        self.consumed += samples.len();
        self.pending.extend_from_slice(samples);

        let mut out = Vec::new();
        while self.pending.len() >= self.chunk {
            let chunk: Vec<f32> = self.pending.drain(..self.chunk).collect();
            self.process_into(chunk, &mut out)?;
        }
        Ok(out)
    }

    /// Drain everything the filter is still holding, so the end of the recording survives.
    ///
    /// Pads the final partial chunk and then feeds silence until the exact expected number of
    /// output samples has come out. Skipping this loses the tail of every recording — at
    /// 44.1 kHz, about 130ms, which is a whole word.
    pub fn flush(&mut self) -> Result<Vec<f32>, ResampleError> {
        if self.inner.is_none() {
            return Ok(Vec::new());
        }

        let target = self.expected_output();
        let mut out = Vec::new();

        if !self.pending.is_empty() {
            let mut chunk = std::mem::take(&mut self.pending);
            chunk.resize(self.chunk, 0.0);
            self.process_into(chunk, &mut out)?;
        }

        // Feed silence until the filter has given back everything it owes. Bounded so a
        // resampler that never converges cannot spin here forever.
        let mut guard = 0;
        while self.emitted < target && guard < 64 {
            self.process_into(vec![0.0; self.chunk], &mut out)?;
            guard += 1;
        }

        // Trim the padding back off, so `n` samples in is exactly `n * to / from` out.
        let overshoot = self.emitted.saturating_sub(target);
        out.truncate(out.len().saturating_sub(overshoot));
        self.emitted -= overshoot;
        Ok(out)
    }

    /// Run one chunk, discarding any of the start-up delay still outstanding.
    fn process_into(&mut self, chunk: Vec<f32>, out: &mut Vec<f32>) -> Result<(), ResampleError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        let converted = inner.process(&[chunk], None)?;
        let Some(channel) = converted.into_iter().next() else {
            return Ok(());
        };

        let dropped = self.skip.min(channel.len());
        self.skip -= dropped;
        let useful = &channel[dropped..];
        self.emitted += useful.len();
        out.extend_from_slice(useful);
        Ok(())
    }
}

/// Average interleaved channels down to mono.
///
/// Speech models take one channel, and taking only the first would throw away half the
/// signal on a stereo microphone rather than combining it.
pub fn to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    match channels {
        0 | 1 => interleaved.to_vec(),
        channels => {
            let channels = usize::from(channels);
            interleaved
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, rate: u32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|n| (2.0 * std::f32::consts::PI * frequency * n as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Estimate a tone's frequency by counting zero crossings — independent of the
    /// resampler's own arithmetic, so it cannot agree with a bug.
    fn estimate_frequency(samples: &[f32], rate: u32) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        crossings as f32 * rate as f32 / (2.0 * samples.len() as f32)
    }

    #[test]
    fn matching_rates_pass_through_untouched() {
        let mut resampler = Resampler::new(16_000, 16_000).expect("constructs");
        assert!(resampler.is_pass_through());

        let input = sine(440.0, 16_000, 1_000);
        assert_eq!(resampler.push(&input).expect("push"), input);
        assert!(resampler.flush().expect("flush").is_empty());
    }

    #[test]
    fn downsampling_preserves_the_tone() {
        // The failure this catches is aliasing: decimating without filtering turns a 3 kHz
        // tone into a different, lower one, and speech into mush.
        let mut resampler = Resampler::new(48_000, 16_000).expect("constructs");
        let input = sine(1_000.0, 48_000, 48_000);

        let mut out = resampler.push(&input).expect("push");
        out.extend(resampler.flush().expect("flush"));

        let measured = estimate_frequency(&out[1_000..out.len() - 1_000], 16_000);
        assert!(
            (measured - 1_000.0).abs() < 30.0,
            "1 kHz became {measured} Hz after downsampling"
        );
    }

    #[test]
    fn the_output_length_is_exact_not_approximate() {
        // Approximately right is not good enough: a shortfall lands at the start of the
        // recording, which is the leading syllable the pre-roll buffer exists to preserve.
        for (from, to, input) in [
            (48_000u32, 16_000u32, 48_000usize),
            (44_100, 16_000, 44_100),
            (8_000, 16_000, 8_000),
            (16_000, 48_000, 16_000),
        ] {
            let mut resampler = Resampler::new(from, to).expect("constructs");
            let mut out = resampler.push(&sine(440.0, from, input)).expect("push");
            out.extend(resampler.flush().expect("flush"));

            let expected = input * to as usize / from as usize;
            assert_eq!(
                out.len(),
                expected,
                "{from} Hz to {to} Hz: {input} samples in produced {} out, expected {expected}",
                out.len()
            );
        }
    }

    #[test]
    fn the_output_starts_where_the_input_does() {
        // The filter has a start-up delay. Leaving it in place would begin every recording
        // with a fraction of a second of nothing and shift the audio by that much.
        let mut resampler = Resampler::new(44_100, 16_000).expect("constructs");
        let mut out = resampler
            .push(&sine(1_000.0, 44_100, 44_100))
            .expect("push");
        out.extend(resampler.flush().expect("flush"));

        // Within the first 5ms there should already be signal, not filter ramp-up.
        let opening = &out[..80];
        let peak = opening.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!(
            peak > 0.1,
            "the recording opens with {peak} peak amplitude — the delay was not compensated"
        );
    }

    #[test]
    fn upsampling_preserves_the_tone() {
        // Cheap microphones do exist at 8 kHz, and the stored format is whatever the user
        // configured.
        let mut resampler = Resampler::new(8_000, 16_000).expect("constructs");
        let mut out = resampler.push(&sine(300.0, 8_000, 8_000)).expect("push");
        out.extend(resampler.flush().expect("flush"));

        let measured = estimate_frequency(&out[1_000..out.len() - 1_000], 16_000);
        assert!(
            (measured - 300.0).abs() < 15.0,
            "300 Hz became {measured} Hz"
        );
    }

    #[test]
    fn a_non_integer_ratio_is_handled() {
        // 44.1 kHz to 16 kHz is 2.75625, which no amount of dropping samples achieves.
        let mut resampler = Resampler::new(44_100, 16_000).expect("constructs");
        let mut out = resampler
            .push(&sine(1_000.0, 44_100, 44_100))
            .expect("push");
        out.extend(resampler.flush().expect("flush"));

        let measured = estimate_frequency(&out[1_000..out.len() - 1_000], 16_000);
        assert!((measured - 1_000.0).abs() < 30.0, "got {measured} Hz");
    }

    #[test]
    fn arbitrary_buffer_sizes_give_the_same_result_as_one_large_one() {
        // A real backend delivers whatever size it likes, and often varies it.
        let input = sine(500.0, 48_000, 24_000);

        let mut whole = Resampler::new(48_000, 16_000).expect("constructs");
        let mut expected = whole.push(&input).expect("push");
        expected.extend(whole.flush().expect("flush"));

        let mut piecemeal = Resampler::new(48_000, 16_000).expect("constructs");
        let mut actual = Vec::new();
        for (index, chunk) in input.chunks(377).enumerate() {
            let _ = index;
            actual.extend(piecemeal.push(chunk).expect("push"));
        }
        actual.extend(piecemeal.flush().expect("flush"));

        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected.iter()) {
            assert!((a - b).abs() < 1e-5, "{a} != {b}");
        }
    }

    #[test]
    fn flushing_recovers_the_tail_that_never_filled_a_chunk() {
        // Without this, every recording loses up to a quarter of a second — a whole word.
        let mut resampler = Resampler::new(48_000, 16_000).expect("constructs");
        let pushed = resampler.push(&sine(440.0, 48_000, 100)).expect("push");
        assert!(pushed.is_empty(), "100 samples cannot fill a chunk");

        assert!(
            !resampler.flush().expect("flush").is_empty(),
            "the tail was dropped"
        );
    }

    #[test]
    fn stereo_is_averaged_rather_than_half_discarded() {
        // Taking only the first channel throws away half the signal on a stereo microphone.
        let interleaved = [1.0, 0.0, 0.5, 0.5, -1.0, 1.0];
        assert_eq!(to_mono(&interleaved, 2), vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn mono_input_is_left_alone() {
        let mono = [0.1, 0.2, 0.3];
        assert_eq!(to_mono(&mono, 1), mono.to_vec());
        assert_eq!(to_mono(&mono, 0), mono.to_vec());
    }

    #[test]
    fn a_trailing_partial_frame_is_dropped_rather_than_mixed_wrongly() {
        // Half a frame is not a sample; including it would put a click in the audio.
        assert_eq!(to_mono(&[1.0, 1.0, 0.5], 2), vec![1.0]);
    }
}
