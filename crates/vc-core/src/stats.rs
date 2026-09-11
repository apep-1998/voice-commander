//! Turning a pile of recordings into the two numbers the configuration actually turns on.
//!
//! The whole reason for logging timings was that `pre_roll_ms` and `cooldown_ms` cannot be
//! guessed — they depend on how a particular person presses a particular key. After a couple
//! of weeks of real use the recordings say what they should be, and this works it out.
//!
//! Pure: it takes records and returns numbers. Reading them off disk is the caller's job.

use crate::session::{Outcome, SessionRecord, SinkOutcome, StopReason};

/// What a corpus of recordings says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub sessions: usize,
    pub total_audio_ms: u64,
    pub median_duration_ms: u64,
    pub longest_ms: u64,

    /// Sessions that were continued at least once.
    pub continued: usize,
    /// How long after releasing the key the user pressed again, sorted.
    pub resume_delays_ms: Vec<u64>,
    /// How much speech was found in the pre-roll window, sorted.
    pub pre_roll_speech_ms: Vec<u32>,
    /// The configured pre-roll, taken from the most recent session that had one.
    pub configured_pre_roll_ms: Option<u32>,

    /// Recordings cut off by the watchdog, i.e. a release keybind that never fired.
    pub watchdog_stops: usize,
    pub transcribed: usize,
    pub transcription_failures: usize,
    pub median_transcription_ms: u64,
    pub sink_runs: usize,
    pub sink_failures: usize,
    pub partial_sessions: usize,
}

/// A recommendation the numbers support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advice {
    pub setting: &'static str,
    pub message: String,
}

impl Summary {
    pub fn from_sessions(sessions: &[SessionRecord]) -> Self {
        let mut summary = Self {
            sessions: sessions.len(),
            ..Self::default()
        };

        let mut durations = Vec::new();
        let mut transcription_latencies = Vec::new();

        for session in sessions {
            summary.total_audio_ms += session.audio.duration_ms;
            durations.push(session.audio.duration_ms);

            if session.continuations > 0 {
                summary.continued += 1;
            }
            if session.hit_watchdog() {
                summary.watchdog_stops += 1;
            }
            if session.outcome == Outcome::Partial {
                summary.partial_sessions += 1;
            }

            for segment in &session.segments {
                if let Some(delay) = segment.resumed_after_ms {
                    summary.resume_delays_ms.push(delay);
                }
                // Only the first segment of a session has a pre-roll window; later ones are
                // governed by the continuation mode instead.
                if segment.index == 0 && segment.pre_roll_ms > 0 {
                    summary
                        .pre_roll_speech_ms
                        .push(segment.speech_in_pre_roll_ms);
                }
            }
            if session.capture.configured_pre_roll_ms > 0 {
                summary.configured_pre_roll_ms = Some(session.capture.configured_pre_roll_ms);
            }

            if let Some(transcript) = &session.transcript {
                if transcript.error.is_some() {
                    summary.transcription_failures += 1;
                } else {
                    summary.transcribed += 1;
                    transcription_latencies.push(transcript.latency_ms);
                }
            }

            for sink in &session.sinks {
                summary.sink_runs += 1;
                if matches!(sink.outcome, SinkOutcome::Failed { .. }) {
                    summary.sink_failures += 1;
                }
            }
        }

        summary.longest_ms = durations.iter().copied().max().unwrap_or(0);
        summary.median_duration_ms = median(&mut durations);
        summary.median_transcription_ms = median(&mut transcription_latencies);
        summary.resume_delays_ms.sort_unstable();
        summary.pre_roll_speech_ms.sort_unstable();
        summary
    }

    /// The 90th percentile of how long users waited before continuing.
    ///
    /// The percentile rather than the maximum: one continuation after a phone call should not
    /// push the recommendation to thirty seconds.
    pub fn resume_delay_p90(&self) -> Option<u64> {
        percentile(&self.resume_delays_ms, 90)
    }

    pub fn pre_roll_speech_p90(&self) -> Option<u32> {
        percentile(&self.pre_roll_speech_ms, 90)
    }

    /// What the numbers suggest changing.
    ///
    /// Deliberately conservative: it stays quiet until there is enough data to mean anything,
    /// because advice from four recordings is noise wearing a suit.
    pub fn advice(&self) -> Vec<Advice> {
        let mut advice = Vec::new();

        if self.sessions < 10 {
            return advice;
        }

        if let Some(p90) = self.pre_roll_speech_p90() {
            if let Some(configured) = self.configured_pre_roll_ms {
                // Speech filling most of the window means the window is the limit, and
                // whatever was said before it is already lost.
                if p90 * 10 >= configured * 9 {
                    advice.push(Advice {
                        setting: "capture.pre_roll_ms",
                        message: format!(
                            "speech regularly fills the whole {configured}ms pre-roll window \
                             (90th percentile {p90}ms) — words are probably still being \
                             clipped; try {}ms",
                            configured * 2
                        ),
                    });
                } else if p90 == 0 && self.pre_roll_speech_ms.len() >= 10 {
                    advice.push(Advice {
                        setting: "capture.pre_roll_ms",
                        message: format!(
                            "no speech has ever landed in the {configured}ms pre-roll window \
                             — it is only costing memory; try a smaller value or \
                             mode = \"warm\""
                        ),
                    });
                }
            }
        }

        if let Some(p90) = self.resume_delay_p90() {
            if self.resume_delays_ms.len() >= 5 {
                advice.push(Advice {
                    setting: "session.cooldown_ms",
                    message: format!(
                        "continuations arrive up to {p90}ms after release (90th percentile of \
                         {}) — a cooldown_ms comfortably above that catches them all",
                        self.resume_delays_ms.len()
                    ),
                });
            }
        }

        if self.watchdog_stops * 10 >= self.sessions {
            advice.push(Advice {
                setting: "keybind",
                message: format!(
                    "{} of {} recordings were cut off by the watchdog — the release keybind \
                     is not firing reliably; try releasing the key before the modifier, or \
                     trigger = \"toggle\"",
                    self.watchdog_stops, self.sessions
                ),
            });
        }

        if self.sink_failures * 5 >= self.sink_runs && self.sink_runs >= 10 {
            advice.push(Advice {
                setting: "sinks",
                message: format!(
                    "{} of {} callback runs failed — check the daemon log for which",
                    self.sink_failures, self.sink_runs
                ),
            });
        }

        advice
    }
}

fn median(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

/// The `p`th percentile of an already-sorted slice.
fn percentile<T: Copy>(sorted: &[T], p: usize) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    let index = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted.get(index).copied()
}

/// Whether a session ended for a reason worth mentioning in a summary.
pub fn notable(reason: StopReason) -> bool {
    !matches!(reason, StopReason::Released)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioFormat, CaptureMode, GapMode, TriggerMode};
    use crate::session::{
        AudioSummary, CaptureSummary, LevelSummary, Segment, SessionId, SinkRecord,
        TranscriptRecord,
    };
    use time::macros::datetime;

    fn segment(index: u32, pre_roll_ms: u32, speech: u32, resumed: Option<u64>) -> Segment {
        Segment {
            index,
            key_down_at: datetime!(2026-09-11 14:48:12 UTC),
            key_up_at: None,
            pre_roll_ms,
            speech_in_pre_roll_ms: speech,
            start_offset_ms: 0,
            duration_ms: 1_000,
            resumed_after_ms: resumed,
            stop_reason: StopReason::Released,
        }
    }

    fn session(segments: Vec<Segment>) -> SessionRecord {
        SessionRecord {
            v: 1,
            id: SessionId::from_raw("s"),
            profile: "dictate".to_owned(),
            trigger: TriggerMode::PushToTalk,
            started_at: datetime!(2026-09-11 14:48:12 UTC),
            finalized_at: None,
            capture: CaptureSummary {
                mode: CaptureMode::Preroll,
                device: "mic".to_owned(),
                configured_pre_roll_ms: 500,
                gap: GapMode::Keep,
                gap_downgraded_to: None,
            },
            continuations: u32::try_from(
                segments
                    .iter()
                    .filter(|s| s.resumed_after_ms.is_some())
                    .count(),
            )
            .unwrap_or(0),
            segments,
            audio: AudioSummary {
                path: "/a.wav".into(),
                format: AudioFormat::Wav,
                sample_rate: 16_000,
                channels: 1,
                bytes: 1_000,
                duration_ms: 1_000,
            },
            levels: LevelSummary {
                peak_dbfs: -10.0,
                mean_rms_dbfs: -30.0,
                speech_ms: 900,
                silence_ms: 100,
                clipped_samples: 0,
            },
            warnings: Vec::new(),
            transcript: None,
            sinks: Vec::new(),
            outcome: Outcome::Ok,
        }
    }

    #[test]
    fn an_empty_corpus_summarizes_without_dividing_by_zero() {
        let summary = Summary::from_sessions(&[]);
        assert_eq!(summary.sessions, 0);
        assert_eq!(summary.median_duration_ms, 0);
        assert!(summary.advice().is_empty());
    }

    #[test]
    fn continuations_are_counted_and_their_delays_collected() {
        let sessions = vec![
            session(vec![
                segment(0, 500, 0, None),
                segment(1, 0, 0, Some(1_200)),
            ]),
            session(vec![segment(0, 500, 0, None)]),
        ];
        let summary = Summary::from_sessions(&sessions);

        assert_eq!(summary.sessions, 2);
        assert_eq!(summary.continued, 1);
        assert_eq!(summary.resume_delays_ms, vec![1_200]);
    }

    #[test]
    fn only_the_first_segment_contributes_a_pre_roll_measurement() {
        // Later segments are governed by the continuation mode, not by the pre-roll window,
        // so counting them would drag the distribution towards zero.
        let summary = Summary::from_sessions(&[session(vec![
            segment(0, 500, 300, None),
            segment(1, 0, 0, Some(800)),
        ])]);

        assert_eq!(summary.pre_roll_speech_ms, vec![300]);
    }

    #[test]
    fn the_percentile_ignores_one_outlier() {
        // One continuation after a phone call should not push the recommendation to thirty
        // seconds.
        let mut sessions: Vec<SessionRecord> = (0..19)
            .map(|_| session(vec![segment(0, 500, 0, None), segment(1, 0, 0, Some(900))]))
            .collect();
        sessions.push(session(vec![
            segment(0, 500, 0, None),
            segment(1, 0, 0, Some(30_000)),
        ]));

        let summary = Summary::from_sessions(&sessions);
        assert_eq!(summary.resume_delay_p90(), Some(900));
    }

    #[test]
    fn advice_stays_quiet_until_there_is_enough_data() {
        // Advice from four recordings is noise wearing a suit.
        let sessions: Vec<SessionRecord> = (0..9)
            .map(|_| session(vec![segment(0, 500, 490, None)]))
            .collect();
        assert!(Summary::from_sessions(&sessions).advice().is_empty());
    }

    #[test]
    fn a_pre_roll_window_that_is_always_full_is_reported_as_too_short() {
        let sessions: Vec<SessionRecord> = (0..20)
            .map(|_| session(vec![segment(0, 500, 490, None)]))
            .collect();

        let advice = Summary::from_sessions(&sessions).advice();
        let entry = advice
            .iter()
            .find(|a| a.setting == "capture.pre_roll_ms")
            .expect("should advise on pre-roll");
        assert!(entry.message.contains("clipped"), "{}", entry.message);
        assert!(entry.message.contains("1000ms"), "{}", entry.message);
    }

    #[test]
    fn a_pre_roll_window_that_never_catches_anything_is_reported_as_wasted() {
        let sessions: Vec<SessionRecord> = (0..20)
            .map(|_| session(vec![segment(0, 500, 0, None)]))
            .collect();

        let advice = Summary::from_sessions(&sessions).advice();
        let entry = advice
            .iter()
            .find(|a| a.setting == "capture.pre_roll_ms")
            .expect("should advise on pre-roll");
        assert!(
            entry.message.contains("only costing memory"),
            "{}",
            entry.message
        );
    }

    #[test]
    fn frequent_watchdog_stops_are_diagnosed_as_a_keybind_problem() {
        // Not a microphone problem, which is what a user would otherwise assume.
        let mut sessions: Vec<SessionRecord> = (0..15)
            .map(|_| session(vec![segment(0, 500, 100, None)]))
            .collect();
        for session in sessions.iter_mut().take(5) {
            session.segments[0].stop_reason = StopReason::Watchdog;
        }

        let advice = Summary::from_sessions(&sessions).advice();
        let entry = advice
            .iter()
            .find(|a| a.setting == "keybind")
            .expect("should diagnose the keybind");
        assert!(
            entry.message.contains("release keybind"),
            "{}",
            entry.message
        );
        assert!(entry.message.contains("toggle"), "{}", entry.message);
    }

    #[test]
    fn transcription_and_callback_outcomes_are_tallied() {
        let mut ok = session(vec![segment(0, 500, 0, None)]);
        ok.transcript = Some(TranscriptRecord {
            transcriber: "openai".to_owned(),
            path: None,
            chars: 40,
            latency_ms: 900,
            language: None,
            error: None,
        });
        ok.sinks = vec![SinkRecord {
            id: 0,
            name: "a".to_owned(),
            kind: "command".to_owned(),
            outcome: SinkOutcome::Ok,
            latency_ms: 10,
            attempts: 1,
        }];

        let mut bad = session(vec![segment(0, 500, 0, None)]);
        bad.transcript = Some(TranscriptRecord {
            transcriber: "openai".to_owned(),
            path: None,
            chars: 0,
            latency_ms: 30_000,
            language: None,
            error: Some("timed out".to_owned()),
        });
        bad.sinks = vec![SinkRecord {
            id: 0,
            name: "a".to_owned(),
            kind: "command".to_owned(),
            outcome: SinkOutcome::Failed {
                error: "boom".to_owned(),
            },
            latency_ms: 10,
            attempts: 1,
        }];
        bad.outcome = Outcome::Partial;

        let summary = Summary::from_sessions(&[ok, bad]);
        assert_eq!(summary.transcribed, 1);
        assert_eq!(summary.transcription_failures, 1);
        assert_eq!(summary.sink_runs, 2);
        assert_eq!(summary.sink_failures, 1);
        assert_eq!(summary.partial_sessions, 1);
        assert_eq!(summary.median_transcription_ms, 900);
    }
}
