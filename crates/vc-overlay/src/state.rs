//! What the overlay is showing, and how the event stream changes it.
//!
//! Deliberately a plain struct with a `fn apply(&mut self, Event)`: the whole mapping from
//! the daemon's contract to what is on screen lives in one readable place, and it can be
//! tested without a compositor.

use std::time::{Duration, Instant};

use vc_core::event::Event;
use vc_core::session::{SinkOutcome, StopReason};

/// What the ring is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Nothing to show; the overlay is hidden.
    Hidden,
    Listening,
    /// Released, but inside the continuation window.
    Cooling,
    /// Transcription and callbacks.
    Working,
    /// Everything finished; lingering before it disappears.
    Done,
}

/// How the input is behaving, which recolours the whole instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Normal,
    Quiet,
    Clipping,
}

/// One callback, as drawn in the panel.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub id: u32,
    pub name: String,
    pub status: RowStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowStatus {
    Planned,
    Running,
    Ok,
    Failed,
    Skipped,
}

#[derive(Debug)]
pub(crate) struct Overlay {
    pub phase: Phase,
    pub tone: Tone,
    /// Newest-first ring of recent levels, 0.0 to 1.0.
    pub levels: Vec<f32>,
    pub head: usize,
    pub started: Option<Instant>,
    pub cooldown_ends: Option<Instant>,
    pub cooldown_total: Duration,
    pub hide_at: Option<Instant>,
    pub status: String,
    pub detail: String,
    /// What `detail` says when there is nothing wrong — restored when a warning clears.
    base_detail: String,
    pub rows: Vec<Row>,
    pub transcriber: Option<String>,
    pub summary: Option<String>,
    /// Bumped whenever something changed that needs a redraw.
    pub dirty: bool,
}

impl Overlay {
    pub(crate) fn new(bars: usize) -> Self {
        Self {
            phase: Phase::Hidden,
            tone: Tone::Normal,
            levels: vec![0.0; bars],
            head: 0,
            started: None,
            cooldown_ends: None,
            cooldown_total: Duration::from_millis(1500),
            hide_at: None,
            status: String::new(),
            detail: String::new(),
            base_detail: String::new(),
            rows: Vec::new(),
            transcriber: None,
            summary: None,
            dirty: true,
        }
    }

    pub(crate) fn visible(&self) -> bool {
        self.phase != Phase::Hidden
    }

    /// Push one level measurement.
    pub(crate) fn push_level(&mut self, value: f32) {
        self.head = (self.head + 1) % self.levels.len();
        self.levels[self.head] = value.clamp(0.0, 1.0);
        self.dirty = true;
    }

    /// dBFS to a 0..1 bar height. -60 is the floor, -6 is full.
    pub(crate) fn scale_db(db: f32) -> f32 {
        ((db + 60.0) / 54.0).clamp(0.0, 1.0)
    }

    /// Seconds of audio so far, for the centre readout.
    pub(crate) fn elapsed(&self, now: Instant) -> Option<Duration> {
        self.started.map(|s| now.saturating_duration_since(s))
    }

    /// How much of the continuation window is left, as a fraction.
    pub(crate) fn cooldown_fraction(&self, now: Instant) -> f32 {
        let (Some(ends), total) = (self.cooldown_ends, self.cooldown_total.as_secs_f32()) else {
            return 0.0;
        };
        if total <= 0.0 {
            return 0.0;
        }
        let left = ends.saturating_duration_since(now).as_secs_f32();
        (left / total).clamp(0.0, 1.0)
    }

    /// Advance time-driven transitions. Returns true if anything changed.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;

        // Once the user stops speaking the spikes are history, and leaving them frozen on
        // screen reads as a hung meter. They fall away instead.
        if self.phase != Phase::Listening {
            let mut moved = false;
            for value in &mut self.levels {
                if *value > 0.001 {
                    *value *= 0.88;
                    moved = true;
                } else {
                    *value = 0.0;
                }
            }
            if moved {
                changed = true;
            }
        }

        if self.phase == Phase::Cooling {
            if let Some(ends) = self.cooldown_ends {
                if now >= ends {
                    // The window closed without a resume; the pipeline is about to run.
                    self.status = "working".to_owned();
                    self.cooldown_ends = None;
                    changed = true;
                }
            }
        }

        if let Some(at) = self.hide_at {
            if now >= at {
                self.phase = Phase::Hidden;
                self.hide_at = None;
                self.rows.clear();
                self.summary = None;
                self.levels.iter_mut().for_each(|v| *v = 0.0);
                changed = true;
            }
        }

        if changed {
            self.dirty = true;
        }
        changed
    }

    /// Fold one event from the daemon into what is on screen.
    pub(crate) fn apply(&mut self, event: &Event, now: Instant) {
        self.dirty = true;
        match event {
            Event::RecordingStarted { pre_roll_ms, .. } => {
                self.phase = Phase::Listening;
                self.tone = Tone::Normal;
                self.started = Some(now);
                self.cooldown_ends = None;
                self.hide_at = None;
                self.rows.clear();
                self.summary = None;
                self.status = "listening".to_owned();
                self.base_detail = if *pre_roll_ms > 0 {
                    format!("{pre_roll_ms}ms lookback")
                } else {
                    "no lookback".to_owned()
                };
                self.detail = self.base_detail.clone();
            }

            Event::Level {
                rms_dbfs,
                speech,
                clipping,
                ..
            } => {
                self.push_level(Self::scale_db(*rms_dbfs));
                // Clipping outranks quietness: a clipped recording is already damaged, while
                // a quiet one merely transcribes worse.
                self.tone = if *clipping {
                    Tone::Clipping
                } else if !*speech && *rms_dbfs < -45.0 {
                    Tone::Quiet
                } else {
                    Tone::Normal
                };
                if self.phase == Phase::Listening {
                    self.status = match self.tone {
                        Tone::Clipping => "clipping".to_owned(),
                        Tone::Quiet => "too quiet".to_owned(),
                        Tone::Normal => "listening".to_owned(),
                    };
                    self.detail = match self.tone {
                        Tone::Clipping => "lower the input volume".to_owned(),
                        Tone::Quiet => "move closer".to_owned(),
                        // Restored, not left alone: advice that outlives the problem it
                        // described is worse than no advice.
                        Tone::Normal => self.base_detail.clone(),
                    };
                }
            }

            Event::RecordingStopped { reason, .. } => {
                if *reason == StopReason::Watchdog {
                    self.tone = Tone::Clipping;
                    self.status = "watchdog".to_owned();
                    self.detail = "the release keybind did not fire".to_owned();
                }
            }

            Event::CooldownStarted { ms } => {
                // The tone described the input; there is no input now, so the instrument
                // should not stay amber through a pipeline that has nothing to do with it.
                self.tone = Tone::Normal;
                self.phase = Phase::Cooling;
                self.cooldown_total = Duration::from_millis(u64::from(*ms));
                self.cooldown_ends = Some(now + self.cooldown_total);
                self.status = "say more?".to_owned();
                self.detail = "press again to continue".to_owned();
            }

            Event::RecordingResumed { segment, .. } => {
                self.phase = Phase::Listening;
                self.cooldown_ends = None;
                self.status = "still listening".to_owned();
                self.base_detail = format!("segment {}", segment + 1);
                self.detail = self.base_detail.clone();
            }

            Event::SessionCancelled { .. } => {
                self.phase = Phase::Done;
                self.status = "cancelled".to_owned();
                self.detail = String::new();
                self.hide_at = Some(now + Duration::from_millis(700));
            }

            Event::SessionFinalized { total_ms, .. } => {
                self.detail = format!("{:.1}s captured", *total_ms as f64 / 1000.0);
            }

            Event::PipelineStarted {
                transcriber, sinks, ..
            } => {
                self.phase = Phase::Working;
                self.tone = Tone::Normal;
                self.cooldown_ends = None;
                self.transcriber = transcriber.clone();
                self.status = match transcriber {
                    Some(_) => "transcribing".to_owned(),
                    None => "working".to_owned(),
                };
                // Every row up front, dimmed. This is the whole reason `pipeline_started`
                // carries the complete plan.
                self.rows = sinks
                    .iter()
                    .map(|sink| Row {
                        id: sink.id,
                        name: sink.name.clone(),
                        status: RowStatus::Planned,
                        // Until it runs, its type is the most useful thing to show.
                        detail: sink.kind.clone(),
                    })
                    .collect();
            }

            Event::TranscribeDone { chars, .. } => {
                self.status = "callbacks".to_owned();
                self.detail = format!("{chars} chars");
            }

            Event::TranscribeFailed { .. } => {
                self.tone = Tone::Clipping;
                self.status = "transcription failed".to_owned();
            }

            Event::SinkStarted { id, .. } => {
                if let Some(row) = self.rows.iter_mut().find(|row| row.id == *id) {
                    row.status = RowStatus::Running;
                    row.detail.clear();
                }
            }

            Event::SinkFinished {
                id,
                outcome,
                latency_ms,
                ..
            } => {
                if let Some(row) = self.rows.iter_mut().find(|row| row.id == *id) {
                    match outcome {
                        SinkOutcome::Ok => {
                            row.status = RowStatus::Ok;
                            row.detail = format!("{latency_ms}ms");
                        }
                        SinkOutcome::Failed { error } => {
                            row.status = RowStatus::Failed;
                            row.detail = short(error);
                        }
                        SinkOutcome::Skipped { reason } => {
                            row.status = RowStatus::Skipped;
                            row.detail = format!("{reason:?}").to_lowercase();
                        }
                    }
                }
            }

            Event::PipelineFinished { ok, failed, .. } => {
                self.phase = Phase::Done;
                self.status = "done".to_owned();
                self.summary = Some(if *failed > 0 {
                    format!("{ok} ok · {failed} failed")
                } else {
                    format!("{ok} ok")
                });
                self.hide_at = Some(now + Duration::from_millis(1500));
            }

            Event::InputWarning { .. } | Event::Error { .. } => {}

            _ => {
                self.dirty = false;
            }
        }
    }
}

/// Trim an error to something that fits beside a row.
fn short(message: &str) -> String {
    let first = message.lines().next().unwrap_or(message).trim();
    if first.chars().count() <= 22 {
        first.to_owned()
    } else {
        first.chars().take(21).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_core::event::PlannedSink;

    fn overlay() -> Overlay {
        Overlay::new(72)
    }

    #[test]
    fn the_overlay_starts_hidden_and_appears_on_a_recording() {
        let mut o = overlay();
        assert!(!o.visible());

        o.apply(
            &Event::RecordingStarted {
                segment: 0,
                pre_roll_ms: 500,
                device: "mic".to_owned(),
            },
            Instant::now(),
        );
        assert_eq!(o.phase, Phase::Listening);
        assert!(o.detail.contains("500ms"));
    }

    #[test]
    fn release_does_not_hide_it() {
        // The property the whole design turns on: an indicator that hid on release would
        // flicker every time the user paused to think.
        let mut o = overlay();
        let now = Instant::now();
        o.apply(&Event::CooldownStarted { ms: 1500 }, now);

        assert_eq!(o.phase, Phase::Cooling);
        assert!(o.visible());
        assert!(o.cooldown_fraction(now) > 0.9);
    }

    #[test]
    fn a_continuation_returns_to_listening() {
        let mut o = overlay();
        let now = Instant::now();
        o.apply(&Event::CooldownStarted { ms: 1500 }, now);
        o.apply(
            &Event::RecordingResumed {
                segment: 1,
                resumed_after_ms: 900,
            },
            now,
        );

        assert_eq!(o.phase, Phase::Listening);
        assert_eq!(o.cooldown_ends, None);
        assert!(
            o.detail.contains('2'),
            "should name the segment: {}",
            o.detail
        );
    }

    #[test]
    fn every_callback_is_drawn_before_any_of_them_runs() {
        let mut o = overlay();
        o.apply(
            &Event::PipelineStarted {
                transcriber: Some("openai".to_owned()),
                sinks: vec![
                    PlannedSink {
                        id: 0,
                        name: "agent".to_owned(),
                        kind: "command".to_owned(),
                        requires_text: true,
                    },
                    PlannedSink {
                        id: 1,
                        name: "archive".to_owned(),
                        kind: "command".to_owned(),
                        requires_text: false,
                    },
                ],
            },
            Instant::now(),
        );

        assert_eq!(o.rows.len(), 2);
        assert!(o.rows.iter().all(|row| row.status == RowStatus::Planned));
    }

    #[test]
    fn a_callback_resolves_by_id_not_by_position() {
        // The same sink may be listed twice, so the id is what correlates.
        let mut o = overlay();
        let now = Instant::now();
        o.apply(
            &Event::PipelineStarted {
                transcriber: None,
                sinks: vec![
                    PlannedSink {
                        id: 0,
                        name: "a".to_owned(),
                        kind: "command".to_owned(),
                        requires_text: false,
                    },
                    PlannedSink {
                        id: 1,
                        name: "b".to_owned(),
                        kind: "http".to_owned(),
                        requires_text: false,
                    },
                ],
            },
            now,
        );
        o.apply(
            &Event::SinkFinished {
                id: 1,
                name: "b".to_owned(),
                outcome: SinkOutcome::Failed {
                    error: "connection refused".to_owned(),
                },
                latency_ms: 30,
                attempts: 1,
            },
            now,
        );

        assert_eq!(o.rows[0].status, RowStatus::Planned);
        assert_eq!(o.rows[1].status, RowStatus::Failed);
        assert_eq!(o.rows[1].detail, "connection refused");
    }

    #[test]
    fn a_warning_colour_does_not_outlive_the_input_it_described() {
        // Otherwise a single quiet moment leaves the whole pipeline rendered in amber.
        let mut o = overlay();
        let now = Instant::now();
        o.apply(
            &Event::Level {
                rms_dbfs: -58.0,
                peak_dbfs: -50.0,
                speech: false,
                clipping: false,
            },
            now,
        );
        assert_eq!(o.tone, Tone::Quiet);

        o.apply(&Event::CooldownStarted { ms: 1500 }, now);
        assert_eq!(o.tone, Tone::Normal);
    }

    #[test]
    fn the_waveform_falls_away_once_recording_stops() {
        // Frozen spikes read as a hung meter.
        let mut o = overlay();
        let now = Instant::now();
        o.apply(
            &Event::Level {
                rms_dbfs: -10.0,
                peak_dbfs: -6.0,
                speech: true,
                clipping: false,
            },
            now,
        );
        let peak = o.levels[o.head];
        assert!(peak > 0.8);

        o.apply(&Event::CooldownStarted { ms: 1500 }, now);
        for _ in 0..40 {
            o.tick(now);
        }
        assert!(
            o.levels.iter().all(|v| *v < 0.01),
            "the spikes did not decay"
        );
    }

    #[test]
    fn advice_disappears_with_the_problem_it_described() {
        let mut o = overlay();
        let now = Instant::now();
        o.apply(
            &Event::RecordingStarted {
                segment: 0,
                pre_roll_ms: 500,
                device: "m".to_owned(),
            },
            now,
        );
        let healthy = o.detail.clone();

        o.apply(
            &Event::Level {
                rms_dbfs: -58.0,
                peak_dbfs: -52.0,
                speech: false,
                clipping: false,
            },
            now,
        );
        assert_eq!(o.detail, "move closer");

        o.apply(
            &Event::Level {
                rms_dbfs: -18.0,
                peak_dbfs: -10.0,
                speech: true,
                clipping: false,
            },
            now,
        );
        assert_eq!(o.detail, healthy, "the advice outlived the problem");
    }

    #[test]
    fn clipping_outranks_quietness() {
        let mut o = overlay();
        o.apply(
            &Event::Level {
                rms_dbfs: -55.0,
                peak_dbfs: -2.0,
                speech: false,
                clipping: true,
            },
            Instant::now(),
        );
        assert_eq!(o.tone, Tone::Clipping);
    }

    #[test]
    fn a_watchdog_stop_is_shown_as_a_keybind_problem() {
        // Not a microphone problem, which is what a user would otherwise assume.
        let mut o = overlay();
        o.apply(
            &Event::RecordingStopped {
                segment: 0,
                duration_ms: 120_000,
                reason: StopReason::Watchdog,
            },
            Instant::now(),
        );
        assert!(o.detail.contains("keybind"), "{}", o.detail);
    }

    #[test]
    fn a_long_error_is_trimmed_to_fit_beside_its_row() {
        assert_eq!(short("short"), "short");
        assert_eq!(short(&"x".repeat(50)).chars().count(), 22);
        assert_eq!(short("first line\nsecond"), "first line");
    }

    #[test]
    fn the_level_scale_puts_silence_at_the_floor_and_speech_high() {
        assert_eq!(Overlay::scale_db(-100.0), 0.0);
        assert_eq!(Overlay::scale_db(0.0), 1.0);
        assert!(Overlay::scale_db(-20.0) > 0.7);
        assert!(Overlay::scale_db(-55.0) < 0.15);
    }

    #[test]
    fn it_hides_itself_after_the_pipeline_finishes() {
        let mut o = overlay();
        let now = Instant::now();
        o.apply(
            &Event::PipelineFinished {
                ok: 2,
                failed: 0,
                skipped: 0,
                total_ms: 900,
                outcome: vc_core::session::Outcome::Ok,
            },
            now,
        );

        assert_eq!(o.phase, Phase::Done);
        assert!(o.visible());
        // Lingers, then goes.
        assert!(!o.tick(now));
        assert!(o.tick(now + Duration::from_millis(1600)));
        assert!(!o.visible());
    }
}
