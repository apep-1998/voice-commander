//! `voice-commander mic-test`: check the microphone before blaming anything else.
//!
//! The single most common cause of an empty recording is a muted or wrong input, and without
//! this the user's first evidence is a transcript that says nothing. This records for a few
//! seconds and reports what arrived, in terms that point at an action.

use std::time::{Duration, Instant};

use vc_core::config::{DeviceSelector, LevelsConfig};
use vc_core::session::InputWarningKind;

use crate::cpal_backend::CpalHost;
use crate::level::{summary_warning, LevelMeter, LevelTotals};
use crate::source::{channel, AudioError, AudioSource, DeviceInfo};

/// What a microphone check found.
#[derive(Debug, Clone)]
pub struct MicReport {
    pub device: DeviceInfo,
    pub duration: Duration,
    pub peak_dbfs: f32,
    pub mean_rms_dbfs: f32,
    pub speech_ms: u64,
    pub silence_ms: u64,
    pub clipped_samples: u64,
    /// Samples lost because the consumer could not keep up.
    pub dropped_samples: u64,
    pub warning: Option<InputWarningKind>,
}

impl MicReport {
    /// A verdict in the terms a user can act on.
    pub fn verdict(&self) -> String {
        match self.warning {
            Some(InputWarningKind::Silence) => {
                "nothing reached the microphone — check that it is unmuted and that the right \
                 input is selected (`wpctl status`)"
                    .to_owned()
            }
            Some(InputWarningKind::TooQuiet) => format!(
                "audio arrived but is quiet ({:.0} dBFS average) — move closer, or raise the \
                 input volume with `wpctl set-volume @DEFAULT_AUDIO_SOURCE@ 1.2`",
                self.mean_rms_dbfs
            ),
            Some(InputWarningKind::Clipping) => format!(
                "{} samples hit full scale — lower the input volume, or the loud parts will \
                 be distorted",
                self.clipped_samples
            ),
            Some(other) => format!("input problem: {other:?}"),
            None => format!(
                "microphone looks good — {:.0} dBFS average, {:.0} dBFS peak",
                self.mean_rms_dbfs, self.peak_dbfs
            ),
        }
    }
}

/// Record for `duration` and report what arrived.
pub fn run(
    selector: &DeviceSelector,
    sample_rate: u32,
    levels: &LevelsConfig,
    duration: Duration,
) -> Result<MicReport, AudioError> {
    let host = CpalHost::new();
    let mut source = host.open(selector, sample_rate)?;
    let device = source.info();

    // A generous queue: this runs while the user speaks, and dropping samples here would
    // make the report accuse the microphone of a problem this code caused.
    let (sink, mut consumer) = channel(device.sample_rate as usize * 4);
    source.start(sink)?;

    let meter = LevelMeter::new(levels.clone());
    let mut totals = LevelTotals::new(device.sample_rate, levels);
    let mut block = Vec::new();

    let started = Instant::now();
    while started.elapsed() < duration {
        std::thread::sleep(Duration::from_millis(50));
        block.clear();
        consumer.drain(&mut block);
        if block.is_empty() {
            continue;
        }
        totals.add(&block, &meter.measure(&block));
    }

    let dropped = consumer.dropped();
    let failed = source.has_failed();
    source.stop();

    if failed {
        return Err(AudioError::Stream(
            "the capture stream failed while testing".to_owned(),
        ));
    }

    let summary = totals.summarize();
    Ok(MicReport {
        device,
        duration: started.elapsed(),
        peak_dbfs: summary.peak_dbfs,
        mean_rms_dbfs: summary.mean_rms_dbfs,
        speech_ms: summary.speech_ms,
        silence_ms: summary.silence_ms,
        clipped_samples: summary.clipped_samples,
        dropped_samples: dropped,
        // Judged on the whole recording rather than on its quietest block: the pauses
        // between words are quiet by definition, and condemning a healthy microphone for
        // having them sends the user chasing a problem that is not there.
        warning: summary_warning(&summary, levels),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(warning: Option<InputWarningKind>) -> MicReport {
        MicReport {
            device: DeviceInfo {
                name: "test".to_owned(),
                sample_rate: 16_000,
                channels: 1,
            },
            duration: Duration::from_secs(3),
            peak_dbfs: -12.0,
            mean_rms_dbfs: -30.0,
            speech_ms: 2_000,
            silence_ms: 1_000,
            clipped_samples: 0,
            dropped_samples: 0,
            warning,
        }
    }

    #[test]
    fn a_silent_microphone_is_told_to_check_the_mute() {
        // The single most common cause of an empty recording.
        let verdict = report(Some(InputWarningKind::Silence)).verdict();
        assert!(verdict.contains("unmuted"), "{verdict}");
        assert!(
            verdict.contains("wpctl"),
            "should give a command to run: {verdict}"
        );
    }

    #[test]
    fn a_quiet_microphone_is_told_how_to_raise_the_gain() {
        let verdict = report(Some(InputWarningKind::TooQuiet)).verdict();
        assert!(verdict.contains("set-volume"), "{verdict}");
    }

    #[test]
    fn a_healthy_microphone_says_so_with_numbers() {
        let verdict = report(None).verdict();
        assert!(verdict.contains("good"), "{verdict}");
        assert!(verdict.contains("-30"), "should show the level: {verdict}");
    }

    #[test]
    #[ignore = "requires a capture device"]
    fn a_real_microphone_can_be_measured() {
        let levels = LevelsConfig {
            silence_dbfs: -55.0,
            too_quiet_dbfs: -40.0,
            speech_dbfs: -45.0,
            silence_warn_after_ms: 1_000,
        };
        let report = run(
            &DeviceSelector::Default,
            16_000,
            &levels,
            Duration::from_secs(3),
        )
        .expect("microphone check");
        println!("{}", report.verdict());
        println!("{report:#?}");
    }
}
