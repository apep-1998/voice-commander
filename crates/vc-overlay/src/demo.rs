//! A scripted session, for looking at the thing without speaking into a microphone.
//!
//! `--demo` exists because the first question about an overlay is always "what does it look
//! like", and answering it should not require a working capture device, an API key, or a
//! keybind.

use std::time::{Duration, Instant};

use vc_core::event::{Event, PlannedSink};
use vc_core::session::{Outcome, SinkOutcome, SkipReason};

/// One scripted event and when it fires, relative to the start of the loop.
struct Beat {
    at: Duration,
    event: Event,
}

fn beat(ms: u64, event: Event) -> Beat {
    Beat {
        at: Duration::from_millis(ms),
        event,
    }
}

fn sink(id: u32, name: &str, kind: &str, requires_text: bool) -> PlannedSink {
    PlannedSink {
        id,
        name: name.to_owned(),
        kind: kind.to_owned(),
        requires_text,
    }
}

/// Plays a full session on a loop: speech, a warning, a continuation, then callbacks —
/// including one that fails, because that is the state worth seeing.
pub(crate) struct Demo {
    beats: Vec<Beat>,
    next: usize,
    started: Instant,
    length: Duration,
}

impl Demo {
    pub(crate) fn new(now: Instant) -> Self {
        let beats = vec![
            beat(
                200,
                Event::RecordingStarted {
                    segment: 0,
                    pre_roll_ms: 500,
                    device: "StreamCam".to_owned(),
                },
            ),
            beat(
                4_200,
                Event::RecordingStopped {
                    segment: 0,
                    duration_ms: 4_000,
                    reason: vc_core::session::StopReason::Released,
                },
            ),
            beat(4_250, Event::CooldownStarted { ms: 1_500 }),
            // Pressed again inside the window — the same session continues.
            beat(
                5_300,
                Event::RecordingResumed {
                    segment: 1,
                    resumed_after_ms: 1_050,
                },
            ),
            beat(
                8_000,
                Event::RecordingStopped {
                    segment: 1,
                    duration_ms: 2_700,
                    reason: vc_core::session::StopReason::Released,
                },
            ),
            beat(8_050, Event::CooldownStarted { ms: 1_500 }),
            beat(
                9_600,
                Event::SessionFinalized {
                    audio_path: "/tmp/demo/audio.wav".into(),
                    total_ms: 6_700,
                    segments: 2,
                },
            ),
            beat(
                9_650,
                Event::PipelineStarted {
                    transcriber: Some("openai".to_owned()),
                    sinks: vec![
                        sink(0, "agent", "command", true),
                        sink(1, "archive", "command", false),
                        sink(2, "webhook", "http", false),
                        sink(3, "clipboard", "clipboard", true),
                    ],
                },
            ),
            beat(
                10_400,
                Event::TranscribeDone {
                    chars: 63,
                    latency_ms: 750,
                    language: Some("en".to_owned()),
                },
            ),
            beat(
                10_420,
                Event::SinkStarted {
                    id: 0,
                    name: "agent".to_owned(),
                },
            ),
            beat(
                10_430,
                Event::SinkStarted {
                    id: 1,
                    name: "archive".to_owned(),
                },
            ),
            beat(
                10_440,
                Event::SinkStarted {
                    id: 2,
                    name: "webhook".to_owned(),
                },
            ),
            beat(
                10_450,
                Event::SinkStarted {
                    id: 3,
                    name: "clipboard".to_owned(),
                },
            ),
            // They resolve out of order, because they run at once.
            beat(
                10_600,
                Event::SinkFinished {
                    id: 3,
                    name: "clipboard".to_owned(),
                    outcome: SinkOutcome::Ok,
                    latency_ms: 150,
                    attempts: 1,
                },
            ),
            beat(
                10_900,
                Event::SinkFinished {
                    id: 1,
                    name: "archive".to_owned(),
                    outcome: SinkOutcome::Ok,
                    latency_ms: 470,
                    attempts: 1,
                },
            ),
            beat(
                11_500,
                Event::SinkFinished {
                    id: 2,
                    name: "webhook".to_owned(),
                    outcome: SinkOutcome::Failed {
                        error: "connection refused".to_owned(),
                    },
                    latency_ms: 1_060,
                    attempts: 3,
                },
            ),
            beat(
                11_900,
                Event::SinkFinished {
                    id: 0,
                    name: "agent".to_owned(),
                    outcome: SinkOutcome::Ok,
                    latency_ms: 1_480,
                    attempts: 1,
                },
            ),
            beat(
                12_000,
                Event::PipelineFinished {
                    ok: 3,
                    failed: 1,
                    skipped: 0,
                    total_ms: 2_350,
                    outcome: Outcome::Partial,
                },
            ),
        ];

        let length = Duration::from_millis(16_000);
        let _ = SkipReason::NoTranscript; // kept in scope for the variant list

        Self {
            beats,
            next: 0,
            started: now,
            length,
        }
    }

    /// Events that are due, and a synthetic level for the ring.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<Event> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.length {
            // Loop, so it can be left running while the look is argued about.
            self.started = now;
            self.next = 0;
            return vec![Event::SessionCancelled {
                reason: "demo loop".to_owned(),
            }];
        }

        let mut due = Vec::new();
        while self.next < self.beats.len() && self.beats[self.next].at <= elapsed {
            due.push(self.beats[self.next].event.clone());
            self.next += 1;
        }
        due
    }

    /// A speech-shaped level: a syllable envelope over a slower phrase envelope.
    ///
    /// Between 2.0s and 3.0s it drops to a mutter, so the amber "too quiet" state is one of
    /// the things you actually see rather than having to provoke.
    pub(crate) fn level(&self, now: Instant) -> Option<Event> {
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let recording = (0.2..4.2).contains(&elapsed) || (5.3..8.0).contains(&elapsed);
        if !recording {
            return None;
        }

        let quiet = (2.0..3.0).contains(&elapsed);
        let syllable = (elapsed * 24.0).sin().abs().powf(1.6);
        let phrase = 0.55 + 0.45 * (elapsed * 3.3).sin().abs();
        let amplitude = if quiet { 0.05 } else { syllable * phrase };

        let rms_dbfs = -60.0 + amplitude * 54.0;
        Some(Event::Level {
            rms_dbfs,
            peak_dbfs: rms_dbfs + 6.0,
            speech: !quiet && amplitude > 0.35,
            clipping: false,
        })
    }
}
