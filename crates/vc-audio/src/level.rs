//! Turning samples into something a person can read.
//!
//! The user's requirement was not "show that recording is on" but "show that the voice is
//! actually arriving" — that the microphone is not muted, not across the room, not clipping.
//! That is what this produces: a level a meter can draw, plus the classifications behind the
//! warnings.

use vc_core::config::LevelsConfig;
use vc_core::session::{InputWarningKind, LevelSummary};

/// The floor reported instead of negative infinity for digital silence.
///
/// A true zero sample is -inf dBFS, which cannot be drawn on a meter or averaged. Every
/// audio tool picks a floor; this one is below anything a real microphone produces.
pub const SILENCE_FLOOR_DBFS: f32 = -100.0;

/// One block's worth of measurement, as carried by a `level` event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelSnapshot {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    /// Above the speech threshold, as opposed to room tone. This is the bit that says the
    /// user's voice is arriving rather than merely that the stream is open.
    pub speech: bool,
    pub clipping: bool,
    pub clipped_samples: u64,
}

/// Measures blocks of samples against the configured thresholds.
#[derive(Debug, Clone)]
pub struct LevelMeter {
    levels: LevelsConfig,
}

impl LevelMeter {
    pub fn new(levels: LevelsConfig) -> Self {
        Self { levels }
    }

    /// Measure one block. An empty block is silence, not an error.
    pub fn measure(&self, samples: &[f32]) -> LevelSnapshot {
        if samples.is_empty() {
            return LevelSnapshot {
                rms_dbfs: SILENCE_FLOOR_DBFS,
                peak_dbfs: SILENCE_FLOOR_DBFS,
                speech: false,
                clipping: false,
                clipped_samples: 0,
            };
        }

        let mut sum_squares = 0.0f64;
        let mut peak = 0.0f32;
        let mut clipped = 0u64;
        for &sample in samples {
            let magnitude = sample.abs();
            sum_squares += f64::from(sample) * f64::from(sample);
            if magnitude > peak {
                peak = magnitude;
            }
            // At or beyond full scale the true peak has been lost to the converter, so this
            // counts samples that were *at least* clipped.
            if magnitude >= 1.0 {
                clipped += 1;
            }
        }

        let rms = (sum_squares / samples.len() as f64).sqrt() as f32;
        let rms_dbfs = to_dbfs(rms);

        LevelSnapshot {
            rms_dbfs,
            peak_dbfs: to_dbfs(peak),
            speech: rms_dbfs >= self.levels.speech_dbfs,
            clipping: clipped > 0,
            clipped_samples: clipped,
        }
    }

    /// The warning this block warrants, if any.
    ///
    /// Clipping wins over quietness because a clipped recording is already damaged, while a
    /// quiet one merely transcribes worse.
    pub fn warning(&self, snapshot: &LevelSnapshot) -> Option<InputWarningKind> {
        if snapshot.clipping {
            Some(InputWarningKind::Clipping)
        } else if snapshot.peak_dbfs < self.levels.silence_dbfs {
            Some(InputWarningKind::Silence)
        } else if snapshot.rms_dbfs < self.levels.too_quiet_dbfs {
            Some(InputWarningKind::TooQuiet)
        } else {
            None
        }
    }
}

/// The warning a whole recording warrants, judged from its totals.
///
/// Deliberately not "the worst thing any single block did". Every recording contains quiet
/// blocks — the pauses between words are quiet by definition — so taking the worst block
/// would label almost everything as too quiet. A summary judges the recording; the per-block
/// [`LevelMeter::warning`] drives the live indicator, where a transient warning is both
/// appropriate and transient.
pub fn summary_warning(summary: &LevelSummary, levels: &LevelsConfig) -> Option<InputWarningKind> {
    if summary.clipped_samples > 0 {
        Some(InputWarningKind::Clipping)
    } else if summary.peak_dbfs < levels.silence_dbfs {
        Some(InputWarningKind::Silence)
    } else if summary.mean_rms_dbfs < levels.too_quiet_dbfs {
        Some(InputWarningKind::TooQuiet)
    } else {
        None
    }
}

/// Running totals across a whole session, for `session.json`.
#[derive(Debug, Clone)]
pub struct LevelTotals {
    sample_rate: u32,
    samples: u64,
    sum_squares: f64,
    peak: f32,
    speech_samples: u64,
    silence_samples: u64,
    clipped_samples: u64,
    silence_dbfs: f32,
    speech_dbfs: f32,
}

impl LevelTotals {
    pub fn new(sample_rate: u32, levels: &LevelsConfig) -> Self {
        Self {
            sample_rate,
            samples: 0,
            sum_squares: 0.0,
            peak: 0.0,
            speech_samples: 0,
            silence_samples: 0,
            clipped_samples: 0,
            silence_dbfs: levels.silence_dbfs,
            speech_dbfs: levels.speech_dbfs,
        }
    }

    /// Fold in a block that has already been measured.
    ///
    /// Takes the snapshot rather than recomputing, so the summary can never disagree with
    /// the meter the user watched.
    pub fn add(&mut self, samples: &[f32], snapshot: &LevelSnapshot) {
        for &sample in samples {
            self.sum_squares += f64::from(sample) * f64::from(sample);
            let magnitude = sample.abs();
            if magnitude > self.peak {
                self.peak = magnitude;
            }
        }
        self.samples += samples.len() as u64;
        self.clipped_samples += snapshot.clipped_samples;

        let count = samples.len() as u64;
        if snapshot.rms_dbfs >= self.speech_dbfs {
            self.speech_samples += count;
        } else if snapshot.peak_dbfs < self.silence_dbfs {
            self.silence_samples += count;
        }
    }

    pub fn summarize(&self) -> LevelSummary {
        let mean_rms = if self.samples == 0 {
            0.0
        } else {
            (self.sum_squares / self.samples as f64).sqrt() as f32
        };
        LevelSummary {
            peak_dbfs: to_dbfs(self.peak),
            mean_rms_dbfs: to_dbfs(mean_rms),
            speech_ms: self.to_ms(self.speech_samples),
            silence_ms: self.to_ms(self.silence_samples),
            clipped_samples: self.clipped_samples,
        }
    }

    fn to_ms(&self, samples: u64) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            samples * 1000 / u64::from(self.sample_rate)
        }
    }
}

/// Amplitude to dBFS, floored rather than returning negative infinity.
pub fn to_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        SILENCE_FLOOR_DBFS
    } else {
        (20.0 * amplitude.log10()).max(SILENCE_FLOOR_DBFS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels() -> LevelsConfig {
        LevelsConfig {
            silence_dbfs: -55.0,
            too_quiet_dbfs: -40.0,
            speech_dbfs: -45.0,
            silence_warn_after_ms: 1000,
        }
    }

    /// A sine wave at a known amplitude: its RMS is amplitude / sqrt(2), which is what makes
    /// these assertions checkable by hand rather than recorded from the implementation.
    fn sine(amplitude: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|n| amplitude * (n as f32 * 0.1).sin())
            .collect()
    }

    #[test]
    fn full_scale_is_zero_dbfs_and_half_scale_is_six_below() {
        assert!((to_dbfs(1.0) - 0.0).abs() < 0.001);
        assert!((to_dbfs(0.5) + 6.02).abs() < 0.01);
        assert!((to_dbfs(0.1) + 20.0).abs() < 0.01);
    }

    #[test]
    fn digital_silence_reports_the_floor_rather_than_negative_infinity() {
        // Negative infinity cannot be drawn on a meter or averaged into a summary.
        assert_eq!(to_dbfs(0.0), SILENCE_FLOOR_DBFS);
        assert!(to_dbfs(f32::MIN_POSITIVE).is_finite());
    }

    #[test]
    fn rms_of_a_sine_is_its_amplitude_over_root_two() {
        let meter = LevelMeter::new(levels());
        let snapshot = meter.measure(&sine(0.5, 10_000));

        let expected = to_dbfs(0.5 / std::f32::consts::SQRT_2);
        assert!(
            (snapshot.rms_dbfs - expected).abs() < 0.2,
            "got {} dBFS, expected about {expected}",
            snapshot.rms_dbfs
        );
        assert!((snapshot.peak_dbfs - to_dbfs(0.5)).abs() < 0.1);
    }

    #[test]
    fn a_loud_block_counts_as_speech_and_a_quiet_one_does_not() {
        let meter = LevelMeter::new(levels());
        // This is the distinction that lets an indicator say "your voice is arriving"
        // instead of just "the microphone is open".
        assert!(meter.measure(&sine(0.3, 1_000)).speech);
        assert!(!meter.measure(&sine(0.0005, 1_000)).speech);
    }

    #[test]
    fn samples_at_full_scale_are_counted_as_clipped() {
        let meter = LevelMeter::new(levels());
        let mut block = sine(0.5, 1_000);
        block[10] = 1.0;
        block[20] = -1.2;

        let snapshot = meter.measure(&block);
        assert!(snapshot.clipping);
        assert_eq!(snapshot.clipped_samples, 2);
    }

    #[test]
    fn an_empty_block_is_silence_not_an_error() {
        let snapshot = LevelMeter::new(levels()).measure(&[]);
        assert_eq!(snapshot.rms_dbfs, SILENCE_FLOOR_DBFS);
        assert!(!snapshot.speech);
        assert!(!snapshot.clipping);
    }

    #[test]
    fn silence_and_too_quiet_are_reported_as_different_problems() {
        // They call for different actions: one means "unmute", the other "move closer".
        let meter = LevelMeter::new(levels());

        let muted = meter.measure(&vec![0.0; 1_000]);
        assert_eq!(meter.warning(&muted), Some(InputWarningKind::Silence));

        let distant = meter.measure(&sine(0.006, 1_000));
        assert_eq!(meter.warning(&distant), Some(InputWarningKind::TooQuiet));

        let healthy = meter.measure(&sine(0.3, 1_000));
        assert_eq!(meter.warning(&healthy), None);
    }

    #[test]
    fn clipping_outranks_quietness() {
        // A clipped recording is already damaged; a quiet one merely transcribes worse.
        let meter = LevelMeter::new(levels());
        let mut block = vec![0.0f32; 1_000];
        block[0] = 1.0;

        assert_eq!(
            meter.warning(&meter.measure(&block)),
            Some(InputWarningKind::Clipping)
        );
    }

    #[test]
    fn session_totals_accumulate_across_blocks() {
        let meter = LevelMeter::new(levels());
        let mut totals = LevelTotals::new(16_000, &levels());

        // One second of speech, then one second of silence.
        let speech = sine(0.3, 16_000);
        totals.add(&speech, &meter.measure(&speech));
        let quiet = vec![0.0f32; 16_000];
        totals.add(&quiet, &meter.measure(&quiet));

        let summary = totals.summarize();
        assert_eq!(summary.speech_ms, 1_000);
        assert_eq!(summary.silence_ms, 1_000);
        assert!((summary.peak_dbfs - to_dbfs(0.3)).abs() < 0.1);
    }

    #[test]
    fn a_session_with_no_audio_summarizes_without_dividing_by_zero() {
        let summary = LevelTotals::new(16_000, &levels()).summarize();
        assert_eq!(summary.speech_ms, 0);
        assert_eq!(summary.peak_dbfs, SILENCE_FLOOR_DBFS);
        assert!(summary.mean_rms_dbfs.is_finite());
    }

    #[test]
    fn a_recording_is_judged_on_its_totals_not_its_quietest_moment() {
        // Every recording has quiet blocks — the pauses between words. Judging on the worst
        // block would label almost every recording "too quiet", which is what a real
        // three-second microphone check did before this existed.
        let summary = LevelSummary {
            peak_dbfs: -11.8,
            mean_rms_dbfs: -32.1,
            speech_ms: 2_800,
            silence_ms: 200,
            clipped_samples: 0,
        };
        assert_eq!(summary_warning(&summary, &levels()), None);
    }

    #[test]
    fn a_genuinely_quiet_recording_is_still_reported() {
        let summary = LevelSummary {
            peak_dbfs: -38.0,
            mean_rms_dbfs: -52.0,
            speech_ms: 0,
            silence_ms: 3_000,
            clipped_samples: 0,
        };
        assert_eq!(
            summary_warning(&summary, &levels()),
            Some(InputWarningKind::TooQuiet)
        );
    }

    #[test]
    fn a_recording_that_never_rose_above_the_floor_is_silence_not_quietness() {
        // These call for different actions: "unmute it" rather than "move closer".
        let summary = LevelSummary {
            peak_dbfs: -100.0,
            mean_rms_dbfs: -100.0,
            speech_ms: 0,
            silence_ms: 3_000,
            clipped_samples: 0,
        };
        assert_eq!(
            summary_warning(&summary, &levels()),
            Some(InputWarningKind::Silence)
        );
    }

    #[test]
    fn the_summary_counts_the_same_clipping_the_meter_reported() {
        // The summary folds in the snapshot rather than recomputing, so the two can never
        // disagree about what the user was shown.
        let meter = LevelMeter::new(levels());
        let mut totals = LevelTotals::new(16_000, &levels());
        let mut block = sine(0.5, 1_000);
        block[5] = 1.0;

        let snapshot = meter.measure(&block);
        totals.add(&block, &snapshot);

        assert_eq!(totals.summarize().clipped_samples, snapshot.clipped_samples);
    }
}
