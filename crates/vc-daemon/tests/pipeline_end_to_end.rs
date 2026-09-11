//! The whole thing: a session goes in, a transcript and callback results come out.
//!
//! These drive the real pipeline with a real transcriber and real callback processes — no
//! mocks — and then assert on both the files on disk and the events an indicator would have
//! seen. No microphone and no network: the audio is a file written by the test, the
//! transcriber is a shell script, and the callbacks are shell scripts.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::broadcast;
use vc_core::config::{AudioFormat, CaptureMode, Config, GapMode, Layer, TriggerMode};
use vc_core::session::{
    AudioSummary, CaptureSummary, LevelSummary, Outcome, SessionId, SessionRecord, SinkOutcome,
};
use vc_daemon::pipeline::{run, Job};

/// A session directory holding a recording, ready for the pipeline.
struct Fixture {
    dir: TempDir,
    session: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let session = dir.path().join("session");
        std::fs::create_dir_all(&session).expect("create session dir");
        vc_audio::wav::write_mono(&session.join("audio.wav"), &vec![0.1; 16_000], 16_000)
            .expect("write audio");
        Self { dir, session }
    }

    /// Write a shell script. Invoked as `/bin/sh <path>` rather than executed directly:
    /// writing a file and immediately exec'ing it races with other threads forking, and the
    /// kernel answers ETXTBSY.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.dir.path().join(name);
        std::fs::write(&path, format!("{body}\n")).expect("write script");
        path.display().to_string()
    }

    fn record(&self) -> SessionRecord {
        SessionRecord {
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
                path: self.session.join("audio.wav"),
                format: AudioFormat::Wav,
                sample_rate: 16_000,
                channels: 1,
                bytes: 32_044,
                duration_ms: 1_000,
            },
            levels: LevelSummary {
                peak_dbfs: -20.0,
                mean_rms_dbfs: -26.0,
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

    fn path(&self) -> &Path {
        &self.session
    }
}

/// Run the pipeline over `config`'s `dictate` profile, returning the events it emitted.
async fn pipeline(fixture: &Fixture, config_text: &str) -> Vec<vc_core::Envelope> {
    let loaded = Config::from_layers(&[
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("<test>", config_text),
    ])
    .expect("valid configuration");

    let (events, mut rx) = broadcast::channel(256);
    let (transcribers, errors) = vc_stt::Registry::from_config(&loaded.config);
    assert!(errors.is_empty(), "{errors:?}");
    let (sinks, errors) = vc_sinks::Registry::from_config(&loaded.config);
    assert!(errors.is_empty(), "{errors:?}");

    let profile = loaded
        .config
        .profiles
        .get("dictate")
        .expect("the test config defines `dictate`")
        .clone();

    run(Job {
        record: fixture.record(),
        profile_name: "dictate".to_owned(),
        profile,
        config: Arc::new(loaded.config),
        transcribers,
        sinks,
        events,
    })
    .await;

    let mut seen = Vec::new();
    while let Ok(line) = rx.try_recv() {
        seen.push(vc_core::Envelope::from_ndjson(&line).expect("a valid event"));
    }
    seen
}

fn kinds(events: &[vc_core::Envelope]) -> Vec<&'static str> {
    events.iter().map(|event| event.event.kind()).collect()
}

fn find<'a>(events: &'a [vc_core::Envelope], kind: &str) -> Option<&'a vc_core::Envelope> {
    events.iter().find(|event| event.event.kind() == kind)
}

/// Read back the `session.json` the pipeline rewrote.
fn stored(fixture: &Fixture) -> SessionRecord {
    let text = std::fs::read_to_string(fixture.path().join("session.json"))
        .expect("the pipeline should rewrite session.json");
    serde_json::from_str(&text).expect("valid session.json")
}

// ── the whole path ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_is_transcribed_and_delivered_to_every_callback() {
    let fixture = Fixture::new();
    let stt = fixture.script("stt.sh", "echo 'open my calendar'");
    let touch_a = fixture.script("a.sh", "printf '%s' \"$1\" > \"$2/a.out\"");
    let touch_b = fixture.script("b.sh", "printf '%s' \"$VC_TEXT\" > \"$1/b.out\"");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[transcribers.local]
type = "command"
cmd = ["/bin/sh", "{stt}", "{{audio_path}}"]
text = {{ from = "stdout" }}

[sinks.a]
type = "command"
cmd = ["/bin/sh", "{touch_a}", "{{text}}", "{{session_dir}}"]

[sinks.b]
type = "command"
cmd = ["/bin/sh", "{touch_b}", "{{session_dir}}"]

[profiles.dictate]
transcriber = "local"
sinks = ["a", "b"]
"#
        ),
    )
    .await;

    // The transcript was written beside the audio.
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("transcript.txt")).expect("transcript"),
        "open my calendar"
    );

    // Both callbacks ran, and both received the text — one as an argument, one as $VC_TEXT.
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("a.out")).expect("a ran"),
        "open my calendar"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("b.out")).expect("b ran"),
        "open my calendar"
    );

    let record = stored(&fixture);
    assert_eq!(record.outcome, Outcome::Ok);
    assert_eq!(record.sinks.len(), 2);
    assert!(record.sinks.iter().all(|sink| sink.outcome.is_ok()));
    assert_eq!(
        record.transcript.as_ref().map(|t| t.chars),
        Some("open my calendar".len())
    );

    let seen = kinds(&events);
    assert!(seen.contains(&"pipeline_started"), "{seen:?}");
    assert!(seen.contains(&"transcribe_done"), "{seen:?}");
    assert!(seen.contains(&"pipeline_finished"), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_whole_plan_is_announced_before_any_callback_starts() {
    // The contract an indicator depends on: a progress panel has to draw every row, greyed
    // out, before the first callback runs. A list that grows while the user watches it is
    // worse than no list at all.
    let fixture = Fixture::new();
    let noop = fixture.script("noop.sh", "true");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[sinks.one]
type = "command"
cmd = ["/bin/sh", "{noop}"]
[sinks.two]
type = "command"
cmd = ["/bin/sh", "{noop}"]
[sinks.three]
type = "command"
cmd = ["/bin/sh", "{noop}"]
[profiles.dictate]
transcriber = false
sinks = ["one", "two", "three"]
"#
        ),
    )
    .await;

    let seen = kinds(&events);
    let plan_at = seen
        .iter()
        .position(|kind| *kind == "pipeline_started")
        .expect("pipeline_started");
    let first_start = seen
        .iter()
        .position(|kind| *kind == "sink_started")
        .expect("sink_started");
    assert!(plan_at < first_start, "the plan must come first: {seen:?}");

    // And it must list all three, with everything a row needs to be drawn.
    match &find(&events, "pipeline_started").expect("event").event {
        vc_core::Event::PipelineStarted { sinks, .. } => {
            assert_eq!(sinks.len(), 3);
            assert_eq!(
                sinks.iter().map(|sink| sink.id).collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
            assert!(sinks.iter().all(|sink| sink.kind == "command"));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_callback_resolves_exactly_once() {
    // A row left as a spinner forever is the worst failure mode an indicator can have.
    let fixture = Fixture::new();
    let ok = fixture.script("ok.sh", "true");
    let bad = fixture.script("bad.sh", "echo 'boom' >&2; exit 1");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[sinks.good]
type = "command"
cmd = ["/bin/sh", "{ok}"]
[sinks.bad]
type = "command"
cmd = ["/bin/sh", "{bad}"]
[profiles.dictate]
transcriber = false
sinks = ["good", "bad"]
"#
        ),
    )
    .await;

    let mut finished: Vec<u32> = events
        .iter()
        .filter_map(|event| match &event.event {
            vc_core::Event::SinkFinished { id, .. } => Some(*id),
            _ => None,
        })
        .collect();

    // Sorted before comparing: in parallel mode these fire as each callback resolves, which
    // is deliberately non-deterministic. The property is that every planned row resolves
    // exactly once, not that they finish in the order they were listed — the *records* are
    // sorted so an indicator's rows stay put, but the events are not.
    finished.sort_unstable();
    assert_eq!(finished, vec![0, 1], "every planned row must resolve once");
}

// ── failures ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_callback_does_not_stop_the_others_and_is_reported() {
    let fixture = Fixture::new();
    let ok = fixture.script("ok.sh", "printf 'ran' > \"$1/ok.out\"");
    let bad = fixture.script("bad.sh", "echo 'webhook unreachable' >&2; exit 1");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[sinks.bad]
type = "command"
cmd = ["/bin/sh", "{bad}"]
[sinks.good]
type = "command"
cmd = ["/bin/sh", "{ok}", "{{session_dir}}"]
[profiles.dictate]
transcriber = false
sinks = ["bad", "good"]
"#
        ),
    )
    .await;

    assert!(
        fixture.path().join("ok.out").exists(),
        "the other callback still ran"
    );

    let record = stored(&fixture);
    assert_eq!(
        record.outcome,
        Outcome::Partial,
        "one failure is partial, not total"
    );

    match &record.sinks[0].outcome {
        SinkOutcome::Failed { error } => assert!(error.contains("webhook unreachable"), "{error}"),
        other => panic!("expected a failure with a reason, got {other:?}"),
    }

    match &find(&events, "pipeline_finished").expect("event").event {
        vc_core::Event::PipelineFinished { ok, failed, .. } => {
            assert_eq!((*ok, *failed), (1, 1));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_transcriber_skips_the_callbacks_that_needed_text() {
    let fixture = Fixture::new();
    let stt = fixture.script("stt.sh", "echo 'model missing' >&2; exit 1");
    let audio_only = fixture.script("archive.sh", "printf 'ran' > \"$1/archive.out\"");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[transcribers.local]
type = "command"
cmd = ["/bin/sh", "{stt}"]
text = {{ from = "stdout" }}

[sinks.needs_text]
type = "command"
requires_text = true
cmd = ["/bin/sh", "{audio_only}", "{{session_dir}}"]

[sinks.archive]
type = "command"
requires_text = false
cmd = ["/bin/sh", "{audio_only}", "{{session_dir}}"]

[profiles.dictate]
transcriber = "local"
sinks = ["needs_text", "archive"]
"#
        ),
    )
    .await;

    let record = stored(&fixture);
    assert_eq!(
        record.sinks[0].outcome,
        SinkOutcome::Skipped {
            reason: vc_core::session::SkipReason::TranscriptionFailed
        },
        "the reason must distinguish this from a profile that never transcribes"
    );
    assert!(
        record.sinks[1].outcome.is_ok(),
        "audio-only callbacks still run"
    );

    let seen = kinds(&events);
    assert!(seen.contains(&"transcribe_failed"), "{seen:?}");
    // The failure must also surface as an error, so disabling both indicators cannot hide it.
    assert!(seen.contains(&"error"), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_without_a_transcriber_still_runs_its_audio_callbacks() {
    // A first-class case, not a degraded one.
    let fixture = Fixture::new();
    let archive = fixture.script("archive.sh", "printf '%s' \"$1\" > \"$2/got.out\"");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[sinks.archive]
type = "command"
requires_text = false
cmd = ["/bin/sh", "{archive}", "{{audio_path}}", "{{session_dir}}"]
[profiles.dictate]
transcriber = false
sinks = ["archive"]
"#
        ),
    )
    .await;

    let got = std::fs::read_to_string(fixture.path().join("got.out")).expect("archive ran");
    assert!(got.ends_with("audio.wav"), "{got}");

    assert!(!kinds(&events).contains(&"transcribe_started"));
    match &find(&events, "pipeline_started").expect("event").event {
        vc_core::Event::PipelineStarted { transcriber, .. } => assert_eq!(transcriber, &None),
        other => panic!("unexpected event {other:?}"),
    }
    assert_eq!(stored(&fixture).outcome, Outcome::Ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_receives_the_session_metadata_on_standard_input() {
    // So a simple script can use `$1` and a real program can parse JSON, without us choosing
    // for them.
    let fixture = Fixture::new();
    let reader = fixture.script("read.sh", "cat > \"$1/stdin.json\"");

    pipeline(
        &fixture,
        &format!(
            r#"
[sinks.reader]
type = "command"
cmd = ["/bin/sh", "{reader}", "{{session_dir}}"]
[profiles.dictate]
transcriber = false
sinks = ["reader"]
"#
        ),
    )
    .await;

    let piped = std::fs::read_to_string(fixture.path().join("stdin.json")).expect("stdin arrived");
    let parsed: SessionRecord = serde_json::from_str(&piped).expect("valid session JSON");
    assert_eq!(parsed.profile, "dictate");
    assert_eq!(parsed.audio.duration_ms, 1_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transcript_of_silence_skips_text_callbacks_without_being_an_error() {
    // A recording with nothing in it is a real answer, not a failure.
    let fixture = Fixture::new();
    let stt = fixture.script("stt.sh", "true");
    let noop = fixture.script("noop.sh", "true");

    let events = pipeline(
        &fixture,
        &format!(
            r#"
[transcribers.local]
type = "command"
cmd = ["/bin/sh", "{stt}"]
text = {{ from = "stdout" }}
[sinks.needs_text]
type = "command"
requires_text = true
cmd = ["/bin/sh", "{noop}"]
[profiles.dictate]
transcriber = "local"
sinks = ["needs_text"]
"#
        ),
    )
    .await;

    assert!(kinds(&events).contains(&"transcribe_done"), "not a failure");
    assert_eq!(
        stored(&fixture).sinks[0].outcome,
        SinkOutcome::Skipped {
            reason: vc_core::session::SkipReason::NoTranscript
        }
    );
}
