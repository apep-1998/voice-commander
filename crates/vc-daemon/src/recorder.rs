//! The session state machine.
//!
//! Releasing the key does **not** end a session. It opens the continuation window, and
//! pressing again inside that window resumes the same recording — the case where a user
//! says their piece, lets go, and immediately remembers one more thing.
//!
//! ```text
//! start ──► Recording(segment 0)
//!             │ stop
//!             ▼
//!        Cooling ─── start again within cooldown_ms ──► Recording(segment 1) ──► …
//!             │
//!             │ window expires
//!             ▼
//!        Finalized ──► audio, timings, level summary
//! ```
//!
//! Everything here is driven by explicit timestamps rather than by reading a clock, so the
//! whole machine is testable to the millisecond without sleeping. That matters: the cases
//! worth testing — resuming one millisecond inside the window, or one outside — are
//! impossible to provoke reliably any other way.

use std::time::{Duration, Instant};

use time::OffsetDateTime;
use vc_audio::level::{LevelSnapshot, LevelTotals};
use vc_core::config::{CaptureMode, GapMode, LevelsConfig, Profile};
use vc_core::session::{CaptureSummary, LevelSummary, Segment, StopReason};

/// What the caller should do as a result of a transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing changed — for instance stopping something already stopped.
    Nothing,
    /// A new session began.
    Started { segment: u32, pre_roll_ms: u32 },
    /// An existing session picked up where it left off.
    Resumed { segment: u32, resumed_after_ms: u64 },
    /// A segment ended and the continuation window is open.
    Stopped {
        segment: u32,
        duration_ms: u64,
        reason: StopReason,
        cooldown_ms: u32,
    },
    /// The session is over and ready to be written out.
    Finalized,
    /// The session was abandoned; nothing downstream should run.
    Cancelled,
}

/// Where a recording is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording,
    Cooling,
}

/// A finished recording, ready to be stored.
#[derive(Debug)]
pub struct Finished {
    pub audio: Vec<f32>,
    pub segments: Vec<Segment>,
    pub continuations: u32,
    pub levels: LevelSummary,
    pub capture: CaptureSummary,
    pub started_at: OffsetDateTime,
}

/// One segment while it is still being recorded.
#[derive(Debug)]
struct OpenSegment {
    index: u32,
    key_down_at: OffsetDateTime,
    key_down: Instant,
    pre_roll_ms: u32,
    speech_in_pre_roll_ms: u32,
    start_offset_ms: u64,
    resumed_after_ms: Option<u64>,
}

/// Drives one recording from the first keypress to a finished buffer.
#[derive(Debug)]
pub struct Recorder {
    sample_rate: u32,
    capture_mode: CaptureMode,
    device: String,
    configured_pre_roll_ms: u32,
    cooldown: Duration,
    max_recording: Duration,
    max_total: Duration,
    gap: GapMode,
    gap_downgraded_to: Option<GapMode>,
    silence_ms: u32,
    max_segments: u32,

    phase: Phase,
    audio: Vec<f32>,
    /// Audio captured during the continuation window.
    ///
    /// Held aside rather than appended, because whether it belongs to the recording is not
    /// known until the window closes: if the user resumes it is the natural pause in the
    /// middle of their sentence, and if they do not it is a second and a half of them having
    /// already stopped talking.
    gap_audio: Vec<f32>,
    segments: Vec<Segment>,
    open: Option<OpenSegment>,
    stopped_at: Option<Instant>,
    continuations: u32,
    totals: LevelTotals,
    started_at: Option<OffsetDateTime>,
    started: Option<Instant>,
}

impl Recorder {
    pub fn new(profile: &Profile, levels: &LevelsConfig, sample_rate: u32, device: String) -> Self {
        // `keep` needs audio from a device that stayed open. In on_demand mode there is
        // none, so it quietly becomes `drop` — recorded in the session so the user can see
        // why their stitched recording has a jump in it.
        let (gap, gap_downgraded_to) = if profile.capture.mode == CaptureMode::OnDemand
            && profile.continuation.gap == GapMode::Keep
        {
            (GapMode::Drop, Some(GapMode::Drop))
        } else {
            (profile.continuation.gap, None)
        };

        Self {
            sample_rate,
            capture_mode: profile.capture.mode,
            device,
            configured_pre_roll_ms: profile.capture.pre_roll_ms,
            cooldown: Duration::from_millis(u64::from(profile.session.cooldown_ms)),
            max_recording: Duration::from_secs(u64::from(profile.session.max_recording_secs)),
            max_total: Duration::from_secs(u64::from(profile.session.max_total_secs)),
            gap,
            gap_downgraded_to,
            silence_ms: profile.continuation.silence_ms,
            max_segments: profile.continuation.max_segments,

            phase: Phase::Idle,
            audio: Vec::new(),
            gap_audio: Vec::new(),
            segments: Vec::new(),
            open: None,
            stopped_at: None,
            continuations: 0,
            totals: LevelTotals::new(sample_rate, levels),
            started_at: None,
            started: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn segment_index(&self) -> u32 {
        self.open.as_ref().map_or(0, |segment| segment.index)
    }

    /// Audio captured so far, in milliseconds.
    pub fn recorded_ms(&self) -> u64 {
        self.samples_to_ms(self.audio.len())
    }

    /// How long the current segment has been running.
    pub fn segment_elapsed_ms(&self, now: Instant) -> u64 {
        self.open.as_ref().map_or(0, |segment| {
            now.saturating_duration_since(segment.key_down).as_millis() as u64
        })
    }

    /// How much of the continuation window is left.
    pub fn cooldown_remaining_ms(&self, now: Instant) -> u32 {
        let Some(stopped) = self.stopped_at else {
            return 0;
        };
        let elapsed = now.saturating_duration_since(stopped);
        u32::try_from(self.cooldown.saturating_sub(elapsed).as_millis()).unwrap_or(u32::MAX)
    }

    /// Begin recording, or resume the session in its continuation window.
    ///
    /// `pre_roll` is the audio captured before this moment — empty in modes that keep none.
    pub fn start(
        &mut self,
        now: Instant,
        wall: OffsetDateTime,
        pre_roll: Vec<f32>,
        pre_roll_speech_ms: u32,
    ) -> Transition {
        match self.phase {
            // Already recording: a repeated press is not an error, it is a key repeat or a
            // compositor delivering the same event twice.
            Phase::Recording => Transition::Nothing,
            Phase::Cooling => self.resume(now, wall),
            Phase::Idle => {
                let pre_roll_ms = u32::try_from(self.samples_to_ms(pre_roll.len()))
                    .unwrap_or(self.configured_pre_roll_ms);
                self.audio = pre_roll;
                self.started_at = Some(wall - Duration::from_millis(u64::from(pre_roll_ms)));
                self.started = Some(now);
                self.open = Some(OpenSegment {
                    index: 0,
                    key_down_at: wall,
                    key_down: now,
                    pre_roll_ms,
                    speech_in_pre_roll_ms: pre_roll_speech_ms,
                    start_offset_ms: 0,
                    resumed_after_ms: None,
                });
                self.phase = Phase::Recording;
                Transition::Started {
                    segment: 0,
                    pre_roll_ms,
                }
            }
        }
    }

    fn resume(&mut self, now: Instant, wall: OffsetDateTime) -> Transition {
        let resumed_after_ms = self.stopped_at.map_or(0, |stopped| {
            now.saturating_duration_since(stopped).as_millis() as u64
        });

        // Decide what the pause becomes.
        match self.gap {
            // The device stayed open, so the gap is simply the middle of the user's
            // sentence. Keeping it is what makes a continued recording sound like one take.
            GapMode::Keep => {
                let gap = std::mem::take(&mut self.gap_audio);
                self.audio.extend_from_slice(&gap);
            }
            GapMode::Drop => self.gap_audio.clear(),
            GapMode::Silence => {
                self.gap_audio.clear();
                let samples = self.ms_to_samples(u64::from(self.silence_ms));
                self.audio.resize(self.audio.len() + samples, 0.0);
            }
        }

        let index = u32::try_from(self.segments.len()).unwrap_or(u32::MAX);
        let start_offset_ms = self.samples_to_ms(self.audio.len());
        self.continuations += 1;
        self.open = Some(OpenSegment {
            index,
            key_down_at: wall,
            key_down: now,
            // Pre-roll belongs to the start of a session. Within one, the continuation mode
            // already decides what happens to the audio between segments.
            pre_roll_ms: 0,
            speech_in_pre_roll_ms: 0,
            start_offset_ms,
            resumed_after_ms: Some(resumed_after_ms),
        });
        self.stopped_at = None;
        self.phase = Phase::Recording;

        Transition::Resumed {
            segment: index,
            resumed_after_ms,
        }
    }

    /// Feed in captured audio. Where it lands depends on the phase.
    pub fn push(&mut self, samples: &[f32], snapshot: &LevelSnapshot) {
        match self.phase {
            Phase::Recording => {
                self.audio.extend_from_slice(samples);
                self.totals.add(samples, snapshot);
            }
            // Held aside until the window closes decides whether it belongs.
            Phase::Cooling if self.gap == GapMode::Keep => {
                self.gap_audio.extend_from_slice(samples);
            }
            Phase::Cooling | Phase::Idle => {}
        }
    }

    /// End the current segment and open the continuation window.
    ///
    /// Idempotent: a compositor can deliver a release without a matching press, and a
    /// keybind that errors when the user did nothing wrong is worse than one that does
    /// nothing.
    pub fn stop(&mut self, now: Instant, wall: OffsetDateTime, reason: StopReason) -> Transition {
        if self.phase != Phase::Recording {
            return Transition::Nothing;
        }
        let Some(open) = self.open.take() else {
            return Transition::Nothing;
        };

        let start_offset_ms = open.start_offset_ms;
        let duration_ms = self
            .samples_to_ms(self.audio.len())
            .saturating_sub(start_offset_ms);
        let index = open.index;

        self.segments.push(Segment {
            index,
            key_down_at: open.key_down_at,
            key_up_at: Some(wall),
            pre_roll_ms: open.pre_roll_ms,
            speech_in_pre_roll_ms: open.speech_in_pre_roll_ms,
            start_offset_ms,
            duration_ms,
            resumed_after_ms: open.resumed_after_ms,
            stop_reason: reason,
        });

        self.stopped_at = Some(now);
        self.phase = Phase::Cooling;
        self.gap_audio.clear();

        // Reasons that end the session outright rather than opening a window: continuing
        // past a limit that was just hit would make the limit meaningless.
        let final_reason = matches!(
            reason,
            StopReason::TotalLimit | StopReason::SegmentLimit | StopReason::Shutdown
        ) || u32::try_from(self.segments.len()).unwrap_or(u32::MAX)
            >= self.max_segments
            || self.cooldown.is_zero();

        if final_reason {
            self.phase = Phase::Cooling;
            self.stopped_at = Some(now - self.cooldown);
        }

        Transition::Stopped {
            segment: index,
            duration_ms,
            reason,
            cooldown_ms: if final_reason {
                0
            } else {
                u32::try_from(self.cooldown.as_millis()).unwrap_or(u32::MAX)
            },
        }
    }

    /// Advance time. Returns a transition when a deadline has been reached.
    ///
    /// The watchdog here is the reason a missed release keybind is survivable: releasing the
    /// modifier before the key means the compositor never fires the release, and without
    /// this the daemon would record until the disk filled.
    pub fn tick(&mut self, now: Instant, wall: OffsetDateTime) -> Transition {
        match self.phase {
            Phase::Recording => {
                if self.segment_elapsed_ms(now) >= self.max_recording.as_millis() as u64 {
                    return self.stop(now, wall, StopReason::Watchdog);
                }
                if self.recorded_ms() >= self.max_total.as_millis() as u64 {
                    return self.stop(now, wall, StopReason::TotalLimit);
                }
                Transition::Nothing
            }
            Phase::Cooling => {
                if self.cooldown_remaining_ms(now) == 0 {
                    self.phase = Phase::Idle;
                    Transition::Finalized
                } else {
                    Transition::Nothing
                }
            }
            Phase::Idle => Transition::Nothing,
        }
    }

    /// Abandon the session. Nothing downstream runs.
    pub fn cancel(&mut self) -> Transition {
        if self.phase == Phase::Idle && self.segments.is_empty() {
            return Transition::Nothing;
        }
        self.phase = Phase::Idle;
        self.audio.clear();
        self.gap_audio.clear();
        self.segments.clear();
        self.open = None;
        self.stopped_at = None;
        Transition::Cancelled
    }

    /// Consume the recorder and hand back what it captured.
    pub fn finish(self) -> Finished {
        Finished {
            levels: self.totals.summarize(),
            capture: CaptureSummary {
                mode: self.capture_mode,
                device: self.device,
                configured_pre_roll_ms: self.configured_pre_roll_ms,
                gap: self.gap,
                gap_downgraded_to: self.gap_downgraded_to,
            },
            started_at: self.started_at.unwrap_or_else(OffsetDateTime::now_utc),
            audio: self.audio,
            segments: self.segments,
            continuations: self.continuations,
        }
    }

    fn samples_to_ms(&self, samples: usize) -> u64 {
        if self.sample_rate == 0 {
            0
        } else {
            samples as u64 * 1000 / u64::from(self.sample_rate)
        }
    }

    fn ms_to_samples(&self, ms: u64) -> usize {
        usize::try_from(ms * u64::from(self.sample_rate) / 1000).unwrap_or(usize::MAX)
    }
}
