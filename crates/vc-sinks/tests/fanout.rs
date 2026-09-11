//! The callback fan-out.
//!
//! The behaviour tested here is what an indicator draws and what `session.json` records, so
//! these assert on outcomes and on the *order events arrive in* rather than on internals.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vc_core::config::{
    AudioFormat, CaptureMode, GapMode, OnError, RetryConfig, SinkMode, TriggerMode,
};
use vc_core::session::{
    AudioSummary, CaptureSummary, LevelSummary, Outcome, SessionId, SessionRecord, SinkOutcome,
    SkipReason,
};
use vc_sinks::fanout::{Planned, Progress};
use vc_sinks::{Sink, SinkContext, SinkError};

/// A sink that does whatever the test tells it to, and counts its calls.
struct Scripted {
    name: String,
    requires_text: bool,
    calls: Arc<AtomicU32>,
    /// Fail this many times before succeeding. `u32::MAX` never succeeds.
    fail_times: u32,
    transient: bool,
    delay: Duration,
}

impl Scripted {
    fn ok(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            requires_text: false,
            calls: Arc::new(AtomicU32::new(0)),
            fail_times: 0,
            transient: false,
            delay: Duration::ZERO,
        }
    }

    fn failing(name: &str) -> Self {
        Self {
            fail_times: u32::MAX,
            ..Self::ok(name)
        }
    }

    fn flaky(name: &str, fail_times: u32) -> Self {
        Self {
            fail_times,
            transient: true,
            ..Self::ok(name)
        }
    }

    fn needing_text(name: &str) -> Self {
        Self {
            requires_text: true,
            ..Self::ok(name)
        }
    }

    fn slow(name: &str, delay: Duration) -> Self {
        Self {
            delay,
            ..Self::ok(name)
        }
    }

    fn counter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.calls)
    }
}

#[async_trait::async_trait]
impl Sink for Scripted {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "scripted"
    }
    fn requires_text(&self) -> bool {
        self.requires_text
    }
    async fn deliver(&self, _: &SinkContext) -> Result<(), SinkError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if call < self.fail_times {
            return Err(if self.transient {
                SinkError::Transient("try again".to_owned())
            } else {
                SinkError::Failed("nope".to_owned())
            });
        }
        Ok(())
    }
}

fn planned(id: u32, sink: Scripted, on_error: OnError, attempts: u32) -> Planned {
    Planned {
        id,
        sink: Arc::new(sink),
        on_error,
        retry: RetryConfig {
            attempts,
            backoff_ms: 1,
        },
    }
}

fn context(text: Option<&str>) -> Arc<SinkContext> {
    let record = SessionRecord {
        v: 1,
        id: SessionId::from_raw("20260911T144812Z-dictate-2hc8b"),
        profile: "dictate".to_owned(),
        trigger: TriggerMode::PushToTalk,
        started_at: time::macros::datetime!(2026-09-11 14:48:12 UTC),
        finalized_at: None,
        capture: CaptureSummary {
            mode: CaptureMode::Preroll,
            device: "mic".to_owned(),
            configured_pre_roll_ms: 500,
            gap: GapMode::Keep,
            gap_downgraded_to: None,
        },
        segments: Vec::new(),
        continuations: 0,
        audio: AudioSummary {
            path: "/data/sess/audio.wav".into(),
            format: AudioFormat::Wav,
            sample_rate: 16_000,
            channels: 1,
            bytes: 1_000,
            duration_ms: 2_400,
        },
        levels: LevelSummary {
            peak_dbfs: -10.0,
            mean_rms_dbfs: -30.0,
            speech_ms: 2_000,
            silence_ms: 400,
            clipped_samples: 0,
        },
        warnings: Vec::new(),
        transcript: None,
        sinks: Vec::new(),
        outcome: Outcome::Ok,
    };
    Arc::new(SinkContext::new(
        record,
        text.map(str::to_owned),
        text.map(|_| "/data/sess/transcript.txt".into()),
    ))
}

/// What the fan-out reports progress through.
type Reporter = Arc<dyn Fn(Progress) + Send + Sync>;

/// Collects the progress events, in the order they arrive.
fn recorder() -> (Reporter, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let report: Reporter = Arc::new(move |progress| {
        let line = match progress {
            Progress::Started { id, name } => format!("start:{id}:{name}"),
            Progress::Finished { record } => {
                let outcome = match &record.outcome {
                    SinkOutcome::Ok => "ok".to_owned(),
                    SinkOutcome::Failed { .. } => "failed".to_owned(),
                    SinkOutcome::Skipped { reason } => format!("skipped:{reason:?}"),
                };
                format!("finish:{}:{}:{outcome}", record.id, record.name)
            }
        };
        sink.lock().expect("lock").push(line);
    });
    (report, seen)
}

// ── the ordinary case ────────────────────────────────────────────────────────

#[tokio::test]
async fn every_callback_runs_and_reports_its_outcome() {
    let (report, seen) = recorder();
    let records = vc_sinks::run(
        vec![
            planned(0, Scripted::ok("a"), OnError::Ignore, 1),
            planned(1, Scripted::ok("b"), OnError::Ignore, 1),
        ],
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.outcome.is_ok()));

    let seen = seen.lock().expect("lock").clone();
    assert!(seen.contains(&"finish:0:a:ok".to_owned()), "{seen:?}");
    assert!(seen.contains(&"finish:1:b:ok".to_owned()), "{seen:?}");
}

#[tokio::test]
async fn results_come_back_in_a_stable_order() {
    // An indicator draws one row per callback. If the records came back in completion order
    // the rows would shuffle as they resolved.
    let (report, _) = recorder();
    let records = vc_sinks::run(
        vec![
            planned(
                0,
                Scripted::slow("slow", Duration::from_millis(80)),
                OnError::Ignore,
                1,
            ),
            planned(1, Scripted::ok("fast"), OnError::Ignore, 1),
            planned(
                2,
                Scripted::slow("slower", Duration::from_millis(120)),
                OnError::Ignore,
                1,
            ),
        ],
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    assert_eq!(
        records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[tokio::test]
async fn parallel_callbacks_really_do_overlap() {
    // The whole reason for the default: the total wait should be the slowest callback, not
    // the sum of all of them. Three 150ms callbacks must not take 450ms.
    let (report, _) = recorder();
    let started = std::time::Instant::now();

    vc_sinks::run(
        (0..3)
            .map(|id| {
                planned(
                    id,
                    Scripted::slow(&format!("s{id}"), Duration::from_millis(150)),
                    OnError::Ignore,
                    1,
                )
            })
            .collect(),
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    assert!(
        started.elapsed() < Duration::from_millis(400),
        "three 150ms callbacks took {:?}; they ran one after another",
        started.elapsed()
    );
}

#[tokio::test]
async fn sequential_callbacks_run_in_the_order_listed() {
    let (report, seen) = recorder();
    vc_sinks::run(
        vec![
            planned(
                0,
                Scripted::slow("first", Duration::from_millis(40)),
                OnError::Ignore,
                1,
            ),
            planned(1, Scripted::ok("second"), OnError::Ignore, 1),
        ],
        context(Some("hello")),
        SinkMode::Sequential,
        None,
        report,
    )
    .await;

    assert_eq!(
        seen.lock().expect("lock").clone(),
        vec![
            "start:0:first".to_owned(),
            "finish:0:first:ok".to_owned(),
            "start:1:second".to_owned(),
            "finish:1:second:ok".to_owned(),
        ]
    );
}

// ── failure isolation ────────────────────────────────────────────────────────

#[tokio::test]
async fn one_failing_callback_does_not_stop_the_others() {
    // The design constraint the whole module is shaped around: an unreachable webhook must
    // not stop the transcript reaching the clipboard.
    let (report, _) = recorder();
    let records = vc_sinks::run(
        vec![
            planned(0, Scripted::ok("good"), OnError::Ignore, 1),
            planned(1, Scripted::failing("bad"), OnError::Ignore, 1),
            planned(2, Scripted::ok("also_good"), OnError::Ignore, 1),
        ],
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    let (ok, failed, skipped) = vc_sinks::tally(&records);
    assert_eq!((ok, failed, skipped), (2, 1, 0));
}

#[tokio::test]
async fn a_failure_carries_the_reason_so_a_row_can_show_it() {
    let (report, _) = recorder();
    let records = vc_sinks::run(
        vec![planned(0, Scripted::failing("bad"), OnError::Ignore, 1)],
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    match &records[0].outcome {
        SinkOutcome::Failed { error } => assert_eq!(error, "nope"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn fail_session_abandons_the_callbacks_after_it() {
    let (report, _) = recorder();
    let records = vc_sinks::run(
        vec![
            planned(0, Scripted::failing("critical"), OnError::FailSession, 1),
            planned(1, Scripted::ok("later"), OnError::Ignore, 1),
        ],
        context(Some("hello")),
        SinkMode::Sequential,
        None,
        report,
    )
    .await;

    assert!(matches!(records[0].outcome, SinkOutcome::Failed { .. }));
    assert_eq!(
        records[1].outcome,
        SinkOutcome::Skipped {
            reason: SkipReason::EarlierSinkFailed
        },
        "the reason must say why, not just that it did not run"
    );
}

// ── retrying ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_transient_failure_is_retried_and_the_attempts_recorded() {
    let sink = Scripted::flaky("webhook", 2);
    let calls = sink.counter();

    let (report, _) = recorder();
    let records = vc_sinks::run(
        vec![planned(0, sink, OnError::Ignore, 3)],
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    assert!(records[0].outcome.is_ok());
    // "this webhook succeeds on the third try, every time" is worth being able to notice.
    assert_eq!(records[0].attempts, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_deliberate_failure_is_not_retried() {
    let sink = Scripted::failing("script");
    let calls = sink.counter();

    let (report, _) = recorder();
    vc_sinks::run(
        vec![planned(0, sink, OnError::Ignore, 5)],
        context(Some("hello")),
        SinkMode::Parallel,
        None,
        report,
    )
    .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "running a script again that chose to fail just does the wrong thing twice"
    );
}

// ── skipping ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_callback_needing_text_is_skipped_with_a_reason_when_there_is_none() {
    // A greyed row without a reason is indistinguishable from one that silently never ran.
    let (report, seen) = recorder();
    let records = vc_sinks::run(
        vec![
            planned(0, Scripted::needing_text("clipboard"), OnError::Ignore, 1),
            planned(1, Scripted::ok("archive"), OnError::Ignore, 1),
        ],
        context(None),
        SinkMode::Parallel,
        Some(SkipReason::NoTranscript),
        report,
    )
    .await;

    assert_eq!(
        records[0].outcome,
        SinkOutcome::Skipped {
            reason: SkipReason::NoTranscript
        }
    );
    assert!(records[1].outcome.is_ok(), "audio-only callbacks still run");

    // It must still be announced, so the row resolves rather than sitting as a spinner.
    let seen = seen.lock().expect("lock").clone();
    assert!(
        seen.iter()
            .any(|line| line.starts_with("finish:0:clipboard:skipped")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_skipped_callback_is_never_started() {
    let sink = Scripted::needing_text("clipboard");
    let calls = sink.counter();

    let (report, _) = recorder();
    vc_sinks::run(
        vec![planned(0, sink, OnError::Ignore, 1)],
        context(None),
        SinkMode::Parallel,
        Some(SkipReason::NoTranscript),
        report,
    )
    .await;

    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failed_transcription_skips_with_its_own_reason() {
    // Distinct from "there was never a transcriber": the user should be able to tell a
    // profile that does not transcribe from one whose provider was down.
    let (report, _) = recorder();
    let records = vc_sinks::run(
        vec![planned(
            0,
            Scripted::needing_text("clipboard"),
            OnError::Ignore,
            1,
        )],
        context(None),
        SinkMode::Parallel,
        Some(SkipReason::TranscriptionFailed),
        report,
    )
    .await;

    assert_eq!(
        records[0].outcome,
        SinkOutcome::Skipped {
            reason: SkipReason::TranscriptionFailed
        }
    );
}

#[tokio::test]
async fn no_callbacks_is_not_an_error() {
    // "Just archive my voice" is a legitimate profile.
    let (report, _) = recorder();
    let records = vc_sinks::run(Vec::new(), context(None), SinkMode::Parallel, None, report).await;

    assert!(records.is_empty());
    assert_eq!(vc_sinks::tally(&records), (0, 0, 0));
}

// ── the registry ─────────────────────────────────────────────────────────────

#[test]
fn the_registry_builds_command_sinks() {
    let loaded = vc_core::Config::from_layers(&[
        vc_core::config::Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        vc_core::config::Layer::new(
            "config.toml",
            r#"
[sinks.agent]
type = "command"
cmd = ["true"]
[profiles.p]
sinks = ["agent"]
"#,
        ),
    ])
    .expect("valid config");

    let (registry, errors) = vc_sinks::Registry::from_config(&loaded.config);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        registry.get("agent").map(|sink| sink.kind()),
        Some("command")
    );
}

#[test]
fn an_unimplemented_sink_is_named_rather_than_silently_skipped() {
    let config: vc_core::config::SinkConfig =
        toml::from_str("type = \"clipboard\"").expect("parses");

    match vc_sinks::build("clip", &config) {
        Err(error) => {
            let message = error.to_string();
            assert!(message.contains("clip"), "{message}");
            assert!(message.contains("clipboard"), "{message}");
        }
        Ok(_) => panic!("the clipboard sink is not implemented in this PR"),
    }
}

#[test]
fn a_command_sink_defaults_to_not_needing_text() {
    // A script handed a .wav path is perfectly useful without a transcript.
    let config: vc_core::config::SinkConfig =
        toml::from_str("type = \"command\"\ncmd = [\"true\"]").expect("parses");
    assert!(!config.needs_text());
}
