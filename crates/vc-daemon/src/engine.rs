//! The capture engine: the one place that owns the microphone.
//!
//! It runs on its own thread rather than on the async runtime, because opening a device
//! blocks for a hundred milliseconds or so and because the drain loop wants a steady tick
//! rather than to be scheduled against everything else. Commands arrive on a channel, status
//! goes out on a watch, and events go out on the same broadcast every subscriber reads.
//!
//! The audio callback itself is further away still — it only ever writes to a lock-free
//! queue, which this thread drains.

use anyhow::Context;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::sync::{broadcast, watch};
use tracing::{debug, error, info, warn};

use std::sync::Arc;
use vc_audio::level::{LevelMeter, LevelSnapshot};
use vc_audio::source::{channel, AudioSource, SampleSource};
use vc_audio::{CpalHost, CpalSource, PreRollBuffer};

use vc_core::config::{CaptureMode, Config, Profile};
use vc_core::event::{DeviceCloseReason, Event, Stage};
use vc_core::session::{
    AudioSummary, InputWarningKind, Outcome, SessionId, SessionRecord, StopReason,
};
use vc_core::Envelope;
use vc_ipc::protocol::ActivityState;

use crate::recorder::{Phase, Recorder, Transition};
use crate::storage::{write_record, Storage};

/// How often the engine wakes to drain audio and check deadlines.
///
/// Fine enough that the watchdog and the cooldown window are accurate to well under the
/// tolerance a person can perceive, and coarse enough to cost nothing while idle.
const TICK: Duration = Duration::from_millis(10);

/// What the daemon asks the engine to do.
#[derive(Debug)]
pub enum Command {
    Start { profile: String },
    Stop { profile: String },
    Toggle { profile: String },
    Cancel,
    Reconfigure(Box<Config>),
    Shutdown,
}

/// What the engine is doing, for `status`.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub activity: ActivityState,
    pub session: Option<SessionId>,
    pub profile: Option<String>,
    pub device: Option<vc_ipc::protocol::DeviceStatus>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            activity: ActivityState::Idle,
            session: None,
            profile: None,
            device: None,
        }
    }
}

/// A session being recorded.
struct Active {
    id: SessionId,
    profile_name: String,
    profile: Profile,
    recorder: Recorder,
}

/// A finished session, handed to the pipeline.
///
/// The capture thread does not run the pipeline itself: transcription and callbacks are
/// async and can take tens of seconds, and blocking the thread that owns the microphone
/// would mean the next recording could not start until a webhook replied.
pub(crate) struct Handoff {
    pub record: SessionRecord,
    pub profile_name: String,
    pub profile: Profile,
}

/// Start the engine on its own thread.
///
/// Returns an error rather than panicking if the thread cannot be created: the daemon can
/// then report that capture could not start and exit with a message, instead of leaving a
/// backtrace as the user's only clue.
pub fn spawn(
    config: Config,
    storage: Storage,
    events: broadcast::Sender<String>,
    runtime: tokio::runtime::Handle,
) -> anyhow::Result<(
    Sender<Command>,
    watch::Receiver<Status>,
    std::thread::JoinHandle<()>,
)> {
    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (status_tx, status_rx) = watch::channel(Status::default());

    let handle = std::thread::Builder::new()
        .name("vc-capture".to_owned())
        .spawn(move || {
            let (transcribers, stt_errors) = vc_stt::Registry::from_config(&config);
            for error in stt_errors {
                error!(%error, "a transcriber could not be built");
            }
            let (sinks, sink_errors) = vc_sinks::Registry::from_config(&config);
            for error in sink_errors {
                error!(%error, "a callback could not be built");
            }

            Engine {
                shared: Arc::new(config.clone()),
                transcribers,
                sinks,
                runtime,
                config,
                storage,
                events,
                status: status_tx,
                host: CpalHost::new(),
                device: None,
                source: None,
                pre_roll: PreRollBuffer::new(16_000, 0),
                pre_roll_rate: 16_000,
                active: None,
                last_activity: Instant::now(),
                block: Vec::new(),
                last_level: Instant::now(),
                nonce: 0,
            }
            .run(&command_rx);
        })
        .context("starting the capture thread")?;

    Ok((command_tx, status_rx, handle))
}

struct Engine {
    config: Config,
    /// The same configuration, shareable with pipeline tasks without copying it per session.
    shared: Arc<Config>,
    transcribers: vc_stt::Registry,
    sinks: vc_sinks::Registry,
    /// Where pipeline tasks are spawned. The capture thread is not itself async.
    runtime: tokio::runtime::Handle,
    storage: Storage,
    events: broadcast::Sender<String>,
    status: watch::Sender<Status>,
    host: CpalHost,
    device: Option<CpalSource>,
    source: Option<SampleSource>,
    pre_roll: PreRollBuffer,
    pre_roll_rate: u32,
    active: Option<Active>,
    last_activity: Instant,
    block: Vec<f32>,
    last_level: Instant,
    /// Distinguishes sessions started inside the same second.
    nonce: u32,
}

impl Engine {
    fn run(&mut self, commands: &Receiver<Command>) {
        if let Err(error) = self.storage.prepare() {
            error!(%error, "cannot prepare the recordings directory");
        }

        loop {
            match commands.recv_timeout(TICK) {
                Ok(Command::Shutdown) => {
                    self.finish_on_shutdown();
                    self.close_device(DeviceCloseReason::Shutdown);
                    return;
                }
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.close_device(DeviceCloseReason::Shutdown);
                    return;
                }
            }
            self.pump();
            self.tick();
            self.publish_status();
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Start { profile } => self.start(&profile),
            Command::Stop { profile } => self.stop(&profile),
            Command::Toggle { profile } => {
                let recording = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.recorder.phase() == Phase::Recording);
                if recording {
                    self.stop(&profile);
                } else {
                    self.start(&profile);
                }
            }
            Command::Cancel => self.cancel(),
            Command::Reconfigure(config) => {
                let config = *config;
                let (transcribers, stt_errors) = vc_stt::Registry::from_config(&config);
                for error in stt_errors {
                    error!(%error, "a transcriber could not be built");
                }
                let (sinks, sink_errors) = vc_sinks::Registry::from_config(&config);
                for error in sink_errors {
                    error!(%error, "a callback could not be built");
                }
                self.transcribers = transcribers;
                self.sinks = sinks;
                self.shared = Arc::new(config.clone());
                self.config = config;
                // A reload can change the device or the pre-roll window, so the old buffer's
                // contents are no longer lookback for the new configuration.
                self.close_device(DeviceCloseReason::Shutdown);
            }
            Command::Shutdown => {}
        }
    }

    fn profile(&self, name: &str) -> Option<Profile> {
        self.config.profiles.get(name).cloned()
    }

    fn start(&mut self, profile_name: &str) {
        let Some(profile) = self.profile(profile_name) else {
            warn!(profile = profile_name, "no such profile");
            return;
        };

        // Continuing an existing session: the recorder already knows what to do.
        if let Some(active) = self.active.as_mut() {
            if active.profile_name == profile_name {
                let transition = active.recorder.start(Instant::now(), now(), Vec::new(), 0);
                let id = active.id.clone();
                let name = active.profile_name.clone();
                self.announce(&id, &name, transition);
                self.last_activity = Instant::now();
                return;
            }
            // A different profile while one is in flight: finish the first rather than
            // interleaving two recordings on one microphone.
            info!(
                running = active.profile_name,
                requested = profile_name,
                "finalising the session in flight before starting another"
            );
            self.finalize();
        }

        if let Err(error) = self.ensure_device(&profile) {
            error!(%error, "cannot open the capture device");
            self.emit_daemon(Event::Error {
                stage: Stage::Capture,
                message: error.to_string(),
            });
            self.emit_daemon(Event::InputWarning {
                kind: InputWarningKind::NoDevice,
                detail: Some(error.to_string()),
            });
            return;
        }

        // Drain anything the device produced while we were setting up, so the pre-roll
        // window reflects the moment of the keypress.
        self.pump();

        let wanted_pre_roll = if profile.capture.mode == CaptureMode::Preroll {
            profile.capture.pre_roll_ms
        } else {
            0
        };
        let pre_roll = self.pre_roll.take_last_ms(wanted_pre_roll);
        let pre_roll_speech_ms = self.speech_ms_in(&pre_roll);

        let wall = now();
        self.nonce = self.nonce.wrapping_add(1);
        let id = SessionId::new(wall, profile_name, self.nonce);
        let device = self
            .device
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), |device| device.info().name);

        let mut recorder = Recorder::new(
            &profile,
            &self.config.levels,
            self.capture_rate(),
            device.clone(),
        );
        let transition = recorder.start(Instant::now(), wall, pre_roll, pre_roll_speech_ms);

        self.active = Some(Active {
            id: id.clone(),
            profile_name: profile_name.to_owned(),
            profile,
            recorder,
        });
        self.last_activity = Instant::now();
        self.announce(&id, profile_name, transition);
    }

    fn stop(&mut self, profile_name: &str) {
        let Some(active) = self.active.as_mut() else {
            return; // Idempotent: a release without a press is not an error.
        };
        if active.profile_name != profile_name {
            return;
        }
        let transition = active
            .recorder
            .stop(Instant::now(), now(), StopReason::Released);
        let id = active.id.clone();
        let name = active.profile_name.clone();
        self.announce(&id, &name, transition);
        self.last_activity = Instant::now();
    }

    fn cancel(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        active.recorder.cancel();
        self.emit(
            &active.id,
            &active.profile_name,
            Event::SessionCancelled {
                reason: "cancelled by the user".to_owned(),
            },
        );
        self.maybe_close_after_recording(&active.profile);
    }

    /// Drain the capture channel into the pre-roll ring and the active recording.
    fn pump(&mut self) {
        let Some(source) = self.source.as_mut() else {
            return;
        };
        self.block.clear();
        if source.drain(&mut self.block) == 0 {
            return;
        }

        let meter = LevelMeter::new(self.config.levels.clone());
        let snapshot = meter.measure(&self.block);

        self.pre_roll.push(&self.block);
        if let Some(active) = self.active.as_mut() {
            active.recorder.push(&self.block, &snapshot);
        }

        self.report_levels(&meter, &snapshot);
    }

    /// Emit level and warning events, throttled.
    ///
    /// Twenty a second is smooth to watch and cheap to produce. Emitting one per drained
    /// block would be a hundred a second, most of which no indicator could draw.
    fn report_levels(&mut self, meter: &LevelMeter, snapshot: &LevelSnapshot) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.recorder.phase() != Phase::Recording {
            return;
        }
        if !self.config.feedback.enabled || !self.config.feedback.listening {
            return;
        }

        let interval = Duration::from_millis(u64::from(self.config.feedback.level_interval_ms));
        if self.last_level.elapsed() < interval {
            return;
        }
        self.last_level = Instant::now();

        let id = active.id.clone();
        let profile = active.profile_name.clone();
        self.emit(
            &id,
            &profile,
            Event::Level {
                rms_dbfs: snapshot.rms_dbfs,
                peak_dbfs: snapshot.peak_dbfs,
                speech: snapshot.speech,
                clipping: snapshot.clipping,
            },
        );
        if let Some(kind) = meter.warning(snapshot) {
            self.emit(&id, &profile, Event::InputWarning { kind, detail: None });
        }
    }

    /// Advance deadlines: the watchdog, the cooldown window, and the idle device release.
    fn tick(&mut self) {
        if let Some(active) = self.active.as_mut() {
            let transition = active.recorder.tick(Instant::now(), now());
            let id = active.id.clone();
            let name = active.profile_name.clone();
            match transition {
                Transition::Finalized => self.finalize(),
                Transition::Nothing => {}
                other => self.announce(&id, &name, other),
            }
        }

        if let Some(device) = self.device.as_ref() {
            if device.has_failed() {
                warn!("the capture stream died");
                self.emit_daemon(Event::Error {
                    stage: Stage::Capture,
                    message: "the capture stream failed".to_owned(),
                });
                self.close_device(DeviceCloseReason::Lost);
                return;
            }
        }
        self.release_if_idle();
    }

    /// Act on a state transition: emit the matching event, and finalize when due.
    fn announce(&mut self, id: &SessionId, profile: &str, transition: Transition) {
        match transition {
            Transition::Nothing => {}
            Transition::Started {
                segment,
                pre_roll_ms,
            } => {
                let device = self
                    .device
                    .as_ref()
                    .map_or_else(|| "unknown".to_owned(), |d| d.info().name);
                self.emit(
                    id,
                    profile,
                    Event::RecordingStarted {
                        segment,
                        pre_roll_ms,
                        device,
                    },
                );
            }
            Transition::Resumed {
                segment,
                resumed_after_ms,
            } => self.emit(
                id,
                profile,
                Event::RecordingResumed {
                    segment,
                    resumed_after_ms,
                },
            ),
            Transition::Stopped {
                segment,
                duration_ms,
                reason,
                cooldown_ms,
            } => {
                self.emit(
                    id,
                    profile,
                    Event::RecordingStopped {
                        segment,
                        duration_ms,
                        reason,
                    },
                );
                if cooldown_ms > 0 {
                    self.emit(id, profile, Event::CooldownStarted { ms: cooldown_ms });
                }
            }
            Transition::Finalized => self.finalize(),
            Transition::Cancelled => self.emit(
                id,
                profile,
                Event::SessionCancelled {
                    reason: "cancelled".to_owned(),
                },
            ),
        }
    }

    /// Write the finished recording out.
    fn finalize(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let Active {
            id,
            profile_name,
            profile,
            recorder,
        } = active;

        let trigger = profile.trigger;
        let finished = recorder.finish();
        let sample_rate = self.capture_rate();

        if finished.audio.is_empty() {
            debug!(session = %id, "nothing was captured; not writing a session");
            self.maybe_close_after_recording(&profile);
            return;
        }

        let result = (|| -> anyhow::Result<(std::path::PathBuf, u64)> {
            let dir = self.storage.session_dir(&id, finished.started_at)?;
            let audio_path = dir.join("audio.wav");
            let bytes = vc_audio::wav::write_mono(&audio_path, &finished.audio, sample_rate)?;
            Ok((audio_path, bytes))
        })();

        let (audio_path, bytes) = match result {
            Ok(written) => written,
            Err(error) => {
                error!(%error, session = %id, "could not write the recording");
                self.emit_daemon(Event::Error {
                    stage: Stage::Storage,
                    message: error.to_string(),
                });
                self.maybe_close_after_recording(&profile);
                return;
            }
        };

        // The length of the file, which is not the sum of the segments: with `gap = "keep"`
        // the pause between two segments is in the recording but belongs to neither of them.
        // Reporting the sum would make `audio.duration_ms` disagree with the file it
        // describes, and anything computing a bitrate or seeking by time would be wrong.
        let total_ms = finished.audio.len() as u64 * 1000 / u64::from(sample_rate.max(1));
        let segments = u32::try_from(finished.segments.len()).unwrap_or(u32::MAX);

        let record = SessionRecord {
            v: vc_core::EVENT_SCHEMA_VERSION,
            id: id.clone(),
            profile: profile_name.clone(),
            trigger,
            started_at: finished.started_at,
            finalized_at: Some(now()),
            capture: finished.capture,
            segments: finished.segments,
            continuations: finished.continuations,
            audio: AudioSummary {
                path: audio_path.clone(),
                format: self.config.audio.format,
                sample_rate,
                channels: 1,
                bytes,
                duration_ms: total_ms,
            },
            levels: finished.levels,
            warnings: Vec::new(),
            transcript: None,
            sinks: Vec::new(),
            outcome: Outcome::Ok,
        };

        if let Some(parent) = audio_path.parent() {
            if let Err(error) = write_record(parent, &record) {
                error!(%error, "could not write session.json");
            }
        }

        info!(session = %id, ms = total_ms, path = %audio_path.display(), "recording stored");
        self.emit(
            &id,
            &profile_name,
            Event::SessionFinalized {
                audio_path,
                total_ms,
                segments,
            },
        );

        // Hand off and return. The pipeline runs detached so the next recording can start
        // while this one is still uploading — someone who says two things in quick
        // succession should not find the second blocked on the first one's webhook.
        self.dispatch(Handoff {
            record,
            profile_name,
            profile: profile.clone(),
        });

        self.maybe_close_after_recording(&profile);
    }

    /// Send a finished session to the pipeline.
    fn dispatch(&self, handoff: Handoff) {
        let job = crate::pipeline::Job {
            record: handoff.record,
            profile_name: handoff.profile_name,
            profile: handoff.profile,
            config: Arc::clone(&self.shared),
            transcribers: self.transcribers.clone(),
            sinks: self.sinks.clone(),
            events: self.events.clone(),
        };
        self.runtime.spawn(crate::pipeline::run(job));
    }

    fn finish_on_shutdown(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active
                .recorder
                .stop(Instant::now(), now(), StopReason::Shutdown);
        }
        self.finalize();
    }

    // ── the device ──────────────────────────────────────────────────────────

    fn capture_rate(&self) -> u32 {
        self.device
            .as_ref()
            .map_or(self.config.audio.sample_rate, |device| {
                device.info().sample_rate
            })
    }

    fn ensure_device(&mut self, profile: &Profile) -> Result<(), vc_audio::AudioError> {
        if self.device.is_some() {
            return Ok(());
        }

        let mut device = self
            .host
            .open(&profile.capture.device, self.config.audio.sample_rate)?;
        let info = device.info();

        // Sized to absorb the engine being descheduled for a while without losing audio.
        let (sink, source) = channel(info.sample_rate as usize * 2);
        device.start(sink)?;

        self.pre_roll_rate = info.sample_rate;
        self.pre_roll = PreRollBuffer::new(
            info.sample_rate,
            if profile.capture.mode == CaptureMode::Preroll {
                profile.capture.pre_roll_ms
            } else {
                0
            },
        );
        info!(
            device = info.name,
            rate = info.sample_rate,
            "capture device opened"
        );
        self.emit_daemon(Event::DeviceOpened {
            device: info.name,
            sample_rate: info.sample_rate,
            channels: info.channels,
        });

        self.device = Some(device);
        self.source = Some(source);
        Ok(())
    }

    fn close_device(&mut self, reason: DeviceCloseReason) {
        if let Some(mut device) = self.device.take() {
            device.stop();
            self.source = None;
            // Audio from before a device change is not lookback, it is a splice of two
            // different microphones.
            self.pre_roll.clear();
            info!(?reason, "capture device closed");
            self.emit_daemon(Event::DeviceClosed { reason });
        }
    }

    /// In on_demand mode the device exists only for the duration of a recording.
    fn maybe_close_after_recording(&mut self, profile: &Profile) {
        if profile.capture.mode == CaptureMode::OnDemand {
            self.close_device(DeviceCloseReason::RecordingEnded);
        }
    }

    /// Let go of the microphone after a spell of inactivity.
    ///
    /// This is the battery knob: it keeps the zero-latency benefit while the user is working
    /// and stops the device being held open all night for nothing.
    fn release_if_idle(&mut self) {
        if self.device.is_none() || self.active.is_some() {
            return;
        }
        let seconds = self
            .config
            .profiles
            .values()
            .map(|profile| profile.capture.idle_release_secs)
            .min()
            .unwrap_or(0);
        if seconds == 0 {
            return;
        }
        if self.last_activity.elapsed() >= Duration::from_secs(u64::from(seconds)) {
            self.close_device(DeviceCloseReason::Idle);
        }
    }

    fn speech_ms_in(&self, samples: &[f32]) -> u32 {
        if samples.is_empty() {
            return 0;
        }
        let meter = LevelMeter::new(self.config.levels.clone());
        let rate = self.pre_roll_rate.max(1) as usize;
        let block = (rate / 50).max(1); // 20ms blocks
        let speaking = samples
            .chunks(block)
            .filter(|chunk| meter.measure(chunk).speech)
            .count();
        u32::try_from(speaking * block * 1000 / rate).unwrap_or(u32::MAX)
    }

    // ── talking to the rest of the daemon ───────────────────────────────────

    fn publish_status(&self) {
        let status = Status {
            activity: match self.active.as_ref() {
                None => ActivityState::Idle,
                Some(active) => match active.recorder.phase() {
                    Phase::Recording => ActivityState::Recording {
                        segment: active.recorder.segment_index(),
                        elapsed_ms: active.recorder.segment_elapsed_ms(Instant::now()),
                    },
                    Phase::Cooling => ActivityState::Cooling {
                        remaining_ms: active.recorder.cooldown_remaining_ms(Instant::now()),
                    },
                    Phase::Idle => ActivityState::Idle,
                },
            },
            session: self.active.as_ref().map(|active| active.id.clone()),
            profile: self
                .active
                .as_ref()
                .map(|active| active.profile_name.clone()),
            device: self.device.as_ref().map(|device| {
                let info = device.info();
                vc_ipc::protocol::DeviceStatus {
                    name: info.name,
                    sample_rate: info.sample_rate,
                    channels: info.channels,
                    pre_roll_available_ms: self.pre_roll.available_ms(),
                }
            }),
        };
        // Only wakes readers when something actually changed.
        self.status.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status;
                true
            }
        });
    }

    fn emit(&self, session: &SessionId, profile: &str, event: Event) {
        self.send(Envelope::for_session(
            now(),
            session.clone(),
            profile,
            event,
        ));
    }

    fn emit_daemon(&self, event: Event) {
        self.send(Envelope::for_daemon(now(), event));
    }

    fn send(&self, envelope: Envelope) {
        if let Ok(line) = envelope.to_ndjson() {
            let _ = self.events.send(line);
        }
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}
