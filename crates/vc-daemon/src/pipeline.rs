//! What happens after the recording is written: transcription, then the callbacks.
//!
//! Runs as a detached task so the next recording can start while this one is still uploading.
//! That is not an optimisation — a user who says two things in quick succession should not
//! find the second one blocked on the first one's webhook.
//!
//! The event sequence here is the contract an indicator is built against: the complete plan
//! first, then each row resolving independently, then a self-contained summary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;
use tracing::{info, warn};

use vc_core::config::{Config, Profile};
use vc_core::event::{Event, PlannedSink, Stage};
use vc_core::session::{Outcome, SessionId, SessionRecord, SkipReason, TranscriptRecord};
use vc_core::Envelope;
use vc_sinks::fanout::{Planned, Progress};
use vc_sinks::SinkContext;
use vc_stt::{TranscribeRequest, Transcript};

/// Everything the pipeline needs, assembled while the capture thread still had it.
pub struct Job {
    pub record: SessionRecord,
    pub profile_name: String,
    pub profile: Profile,
    pub config: Arc<Config>,
    pub transcribers: vc_stt::Registry,
    pub sinks: vc_sinks::Registry,
    pub events: broadcast::Sender<String>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("session", &self.record.id)
            .field("profile", &self.profile_name)
            .finish_non_exhaustive()
    }
}

/// Transcribe if asked to, then run every callback.
pub async fn run(mut job: Job) {
    let started = Instant::now();
    let session = job.record.id.clone();
    let profile = job.profile_name.clone();
    let emitter = Emitter {
        events: job.events.clone(),
        session: session.clone(),
        profile: profile.clone(),
        filter: crate::feedback::Filter::new(&job.config.feedback),
    };

    let planned = plan(&job);
    emitter.send(Event::PipelineStarted {
        transcriber: job.profile.transcriber.clone(),
        sinks: planned.clone(),
    });

    let (transcript, skip) = transcribe(&job, &emitter).await;

    // Write the transcript beside the audio, so a callback can be handed a path rather than
    // a shell-quoted blob of the user's speech.
    let text_path = match &transcript {
        Some(transcript) if !transcript.text.is_empty() => {
            write_transcript(&job.record, &transcript.text).await
        }
        _ => None,
    };

    job.record.transcript = transcript.as_ref().map(|transcript| TranscriptRecord {
        transcriber: job.profile.transcriber.clone().unwrap_or_default(),
        path: text_path.clone(),
        chars: transcript.text.chars().count(),
        latency_ms: 0,
        language: transcript.language.clone(),
        error: None,
    });

    let context = Arc::new(SinkContext::new(
        job.record.clone(),
        transcript
            .as_ref()
            .map(|transcript| transcript.text.clone()),
        text_path,
    ));

    let records = vc_sinks::run(
        build_plan(&job),
        context,
        job.profile.sink_mode,
        skip,
        sink_reporter(&emitter),
    )
    .await;

    let (ok, failed, skipped) = vc_sinks::tally(&records);

    // "Partial" means the audio was captured but something downstream did not work — a
    // failed callback, or a transcription that did not produce text. The distinction from
    // "ok" is what a user scanning `session.json` files later actually looks for.
    let transcription_failed = job
        .record
        .transcript
        .as_ref()
        .is_some_and(|transcript| transcript.error.is_some());
    let outcome = if failed > 0 || transcription_failed {
        Outcome::Partial
    } else {
        Outcome::Ok
    };

    job.record.sinks = records;
    job.record.outcome = outcome;

    // Rewrite session.json now that the transcript and the callback results are known. A
    // callback already received the earlier version on stdin; this is the one that stays on
    // disk for `stats` to read later.
    if let Some(dir) = job.record.audio.path.parent() {
        if let Err(error) = crate::storage::write_record(dir, &job.record) {
            warn!(%error, "could not update session.json");
        }
    }

    let total_ms = started.elapsed().as_millis() as u64;
    info!(session = %session, ok, failed, skipped, total_ms, "pipeline finished");
    emitter.send(Event::PipelineFinished {
        ok,
        failed,
        skipped,
        total_ms,
        outcome,
    });
}

/// The complete list of callbacks, announced before any of them run.
///
/// A progress panel has to be drawn in full and greyed out first; a list that grows while the
/// user watches it is worse than no list.
fn plan(job: &Job) -> Vec<PlannedSink> {
    job.profile
        .sinks
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let sink = job.sinks.get(name)?;
            Some(PlannedSink {
                id: u32::try_from(index).unwrap_or(u32::MAX),
                name: name.clone(),
                kind: sink.kind().to_owned(),
                requires_text: sink.requires_text(),
            })
        })
        .collect()
}

fn build_plan(job: &Job) -> Vec<Planned> {
    job.profile
        .sinks
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let sink = job.sinks.get(name)?;
            let config = job.config.sinks.get(name)?;
            Some(Planned {
                id: u32::try_from(index).unwrap_or(u32::MAX),
                sink: Arc::clone(sink),
                on_error: config.on_error,
                retry: config.retry.clone(),
            })
        })
        .collect()
}

/// Run the transcriber, if the profile has one.
///
/// Returns the transcript and, when there is none, the reason — so a skipped callback can say
/// *why* rather than merely being greyed out.
async fn transcribe(job: &Job, emitter: &Emitter) -> (Option<Transcript>, Option<SkipReason>) {
    let session = &emitter.session;
    let profile = emitter.profile.as_str();
    let Some(name) = &job.profile.transcriber else {
        // Not a failure: a profile that records straight to a callback is a first-class case.
        return (None, Some(SkipReason::NoTranscript));
    };

    let Some(transcriber) = job.transcribers.get(name) else {
        warn!(
            transcriber = name,
            "transcriber is configured but was not built"
        );
        emitter.send(Event::TranscribeFailed {
            error: format!("transcriber {name:?} could not be built"),
            latency_ms: 0,
        });
        return (None, Some(SkipReason::TranscriptionFailed));
    };

    emitter.send(Event::TranscribeStarted {
        transcriber: name.clone(),
    });

    let request = TranscribeRequest {
        audio_path: job.record.audio.path.clone(),
        session_dir: job
            .record
            .audio
            .path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default(),
        session_id: session.clone(),
        profile: profile.to_owned(),
        sample_rate: job.record.audio.sample_rate,
        duration_ms: job.record.audio.duration_ms,
    };

    let started = Instant::now();
    let config = job.config.transcribers.get(name);
    let policy = config
        .map(|config| config.retry.clone())
        .unwrap_or_default();

    let (result, _attempts) = vc_exec::retry(&policy, name, |_| async {
        match transcriber.transcribe(&request).await {
            Ok(transcript) => vc_exec::RetryOutcome::Done(transcript),
            Err(error) if error.is_transient() => vc_exec::RetryOutcome::Retry(error),
            Err(error) => vc_exec::RetryOutcome::Fatal(error),
        }
    })
    .await;

    let latency_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(transcript) => {
            info!(session = %session, chars = transcript.text.chars().count(), latency_ms, "transcribed");
            emitter.send(Event::TranscribeDone {
                chars: transcript.text.chars().count(),
                latency_ms,
                language: transcript.language.clone(),
            });
            // An empty transcript is a real answer — a recording of silence has no text in
            // it — but a callback expecting text still has nothing to work with.
            let skip = transcript
                .text
                .is_empty()
                .then_some(SkipReason::NoTranscript);
            (Some(transcript), skip)
        }
        Err(error) => {
            warn!(session = %session, %error, latency_ms, "transcription failed");
            emitter.send(Event::TranscribeFailed {
                error: error.to_string(),
                latency_ms,
            });
            emitter.send(Event::Error {
                stage: Stage::Transcription,
                message: error.to_string(),
            });
            (None, Some(SkipReason::TranscriptionFailed))
        }
    }
}

async fn write_transcript(record: &SessionRecord, text: &str) -> Option<PathBuf> {
    let dir = record.audio.path.parent()?;
    let path = dir.join("transcript.txt");
    match tokio::fs::write(&path, text).await {
        Ok(()) => Some(path),
        Err(error) => {
            warn!(%error, path = %path.display(), "could not write the transcript");
            None
        }
    }
}

/// Turn fan-out progress into events.
fn sink_reporter(emitter: &Emitter) -> Arc<dyn Fn(Progress) + Send + Sync> {
    let emitter = emitter.clone();
    Arc::new(move |progress| {
        emitter.send(match progress {
            Progress::Started { id, name } => Event::SinkStarted { id, name },
            Progress::Finished { record } => Event::SinkFinished {
                id: record.id,
                name: record.name,
                outcome: record.outcome,
                latency_ms: record.latency_ms,
                attempts: record.attempts,
            },
        });
    })
}

/// Emits events for one session, already knowing which of them are wanted.
///
/// Carrying the session, the profile and the feedback filter together means no call site has
/// to remember all three, and no path can accidentally bypass the filter.
#[derive(Clone)]
struct Emitter {
    events: broadcast::Sender<String>,
    session: SessionId,
    profile: String,
    filter: crate::feedback::Filter,
}

impl Emitter {
    fn send(&self, event: Event) {
        if !self.filter.allows(&event) {
            return;
        }
        if let Ok(line) = Envelope::for_session(
            time::OffsetDateTime::now_utc(),
            self.session.clone(),
            self.profile.clone(),
            event,
        )
        .to_ndjson()
        {
            let _ = self.events.send(line);
        }
    }
}
