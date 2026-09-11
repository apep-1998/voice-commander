//! `session.json` is handed to every callback and is the corpus `voice-commander stats` will
//! later read, so its shape is as much a contract as the event stream.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the shared helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used)]

use time::macros::datetime;
use vc_core::config::{AudioFormat, CaptureMode, GapMode, TriggerMode};
use vc_core::session::{
    AudioSummary, CaptureSummary, InputWarningKind, LevelSummary, Outcome, Segment, SessionId,
    SessionRecord, SinkOutcome, SinkRecord, SkipReason, StopReason, TranscriptRecord,
};

fn segment(index: u32, duration_ms: u64, stop_reason: StopReason) -> Segment {
    Segment {
        index,
        key_down_at: datetime!(2026-09-11 14:48:12 UTC),
        key_up_at: Some(datetime!(2026-09-11 14:48:14 UTC)),
        pre_roll_ms: 500,
        speech_in_pre_roll_ms: 0,
        start_offset_ms: 0,
        duration_ms,
        resumed_after_ms: None,
        stop_reason,
    }
}

fn record(segments: Vec<Segment>) -> SessionRecord {
    SessionRecord {
        v: vc_core::EVENT_SCHEMA_VERSION,
        id: SessionId::from_raw("20260911T144812Z-dictate-2hc8b"),
        profile: "dictate".to_owned(),
        trigger: TriggerMode::PushToTalk,
        started_at: datetime!(2026-09-11 14:48:12 UTC),
        finalized_at: Some(datetime!(2026-09-11 14:48:16 UTC)),
        capture: CaptureSummary {
            mode: CaptureMode::Preroll,
            device: "Wireless Microphone RX".to_owned(),
            configured_pre_roll_ms: 500,
            gap: GapMode::Keep,
            gap_downgraded_to: None,
        },
        segments,
        continuations: 0,
        audio: AudioSummary {
            path: "/data/audio.wav".into(),
            format: AudioFormat::Wav,
            sample_rate: 16_000,
            channels: 1,
            bytes: 64_000,
            duration_ms: 2_000,
        },
        levels: LevelSummary {
            peak_dbfs: -8.0,
            mean_rms_dbfs: -28.0,
            speech_ms: 1_800,
            silence_ms: 200,
            clipped_samples: 0,
        },
        warnings: vec![],
        transcript: None,
        sinks: vec![],
        outcome: Outcome::Ok,
    }
}

#[test]
fn a_record_survives_a_round_trip_through_json() {
    let original = record(vec![segment(0, 2_000, StopReason::Released)]);
    let json = serde_json::to_string(&original).expect("serializes");
    let parsed: SessionRecord = serde_json::from_str(&json).expect("parses back");
    assert_eq!(parsed, original);
}

#[test]
fn timestamps_are_rfc3339_so_any_tool_can_read_them() {
    let json = serde_json::to_string(&record(vec![])).expect("serializes");
    assert!(
        json.contains(r#""started_at":"2026-09-11T14:48:12Z""#),
        "{json}"
    );
}

#[test]
fn total_duration_sums_every_segment() {
    let record = record(vec![
        segment(0, 2_000, StopReason::Released),
        segment(1, 1_500, StopReason::Released),
    ]);
    assert_eq!(record.total_duration_ms(), 3_500);
}

#[test]
fn a_watchdog_stop_is_detectable_without_reading_every_segment() {
    // A user hitting this repeatedly has a keybind problem, and the daemon should be able to
    // tell them so rather than leaving them to notice 120-second recordings.
    let normal = record(vec![segment(0, 100, StopReason::Released)]);
    assert!(!normal.hit_watchdog());

    let stuck = record(vec![
        segment(0, 100, StopReason::Released),
        segment(1, 120_000, StopReason::Watchdog),
    ]);
    assert!(stuck.hit_watchdog());
}

#[test]
fn a_continuation_records_the_tuning_numbers() {
    // These two fields are the whole reason for logging: they answer "is cooldown_ms long
    // enough" and "is pre_roll_ms long enough" from real use instead of from guesswork.
    let mut continued = segment(1, 1_000, StopReason::Released);
    continued.resumed_after_ms = Some(1_420);
    continued.speech_in_pre_roll_ms = 310;

    let json = serde_json::to_string(&continued).expect("serializes");
    assert!(json.contains(r#""resumed_after_ms":1420"#), "{json}");
    assert!(json.contains(r#""speech_in_pre_roll_ms":310"#), "{json}");
}

#[test]
fn a_first_segment_omits_the_resume_delay_rather_than_reporting_zero() {
    // Zero would be indistinguishable from an instantaneous continuation when the data is
    // later aggregated.
    let json = serde_json::to_string(&segment(0, 100, StopReason::Released)).expect("serializes");
    assert!(!json.contains("resumed_after_ms"), "{json}");
}

#[test]
fn a_downgraded_gap_mode_is_recorded() {
    // `gap = "keep"` cannot be honoured in on_demand mode. Silently doing something else
    // would leave the user unable to explain why their stitched recording has a jump in it.
    let mut record = record(vec![]);
    record.capture.mode = CaptureMode::OnDemand;
    record.capture.gap_downgraded_to = Some(GapMode::Drop);

    let json = serde_json::to_string(&record).expect("serializes");
    assert!(json.contains(r#""gap_downgraded_to":"drop""#), "{json}");
}

#[test]
fn sink_outcomes_serialize_as_a_closed_set() {
    let sinks = vec![
        SinkRecord {
            id: 0,
            name: "agent".to_owned(),
            kind: "command".to_owned(),
            outcome: SinkOutcome::Ok,
            latency_ms: 800,
            attempts: 1,
        },
        SinkRecord {
            id: 1,
            name: "clipboard".to_owned(),
            kind: "clipboard".to_owned(),
            outcome: SinkOutcome::Skipped {
                reason: SkipReason::NoTranscript,
            },
            latency_ms: 0,
            attempts: 0,
        },
    ];
    let json = serde_json::to_string(&sinks).expect("serializes");
    assert!(json.contains(r#""outcome":{"status":"ok"}"#), "{json}");
    assert!(
        json.contains(r#""outcome":{"status":"skipped","reason":"no_transcript"}"#),
        "{json}"
    );
}

#[test]
fn a_failed_transcription_is_recorded_alongside_its_latency() {
    // Knowing a provider took 30 seconds to fail is what tells a user to lower the timeout.
    let mut record = record(vec![]);
    record.transcript = Some(TranscriptRecord {
        transcriber: "openai".to_owned(),
        path: None,
        chars: 0,
        latency_ms: 30_000,
        language: None,
        error: Some("request timed out".to_owned()),
    });
    record.outcome = Outcome::Partial;

    let json = serde_json::to_string(&record).expect("serializes");
    let parsed: SessionRecord = serde_json::from_str(&json).expect("parses");
    let transcript = parsed.transcript.expect("transcript record");
    assert_eq!(transcript.latency_ms, 30_000);
    assert_eq!(transcript.error.as_deref(), Some("request timed out"));
}

#[test]
fn input_warnings_are_kept_with_the_session_that_raised_them() {
    let mut record = record(vec![]);
    record.warnings = vec![InputWarningKind::TooQuiet, InputWarningKind::Clipping];

    let json = serde_json::to_string(&record).expect("serializes");
    assert!(
        json.contains(r#""warnings":["too_quiet","clipping"]"#),
        "{json}"
    );
}

#[test]
fn an_older_record_without_optional_fields_still_parses() {
    // Recordings outlive the version of the daemon that wrote them, and `stats` has to read
    // the whole archive.
    let minimal = r#"{
        "v": 1,
        "id": "20260911T144812Z-dictate-2hc8b",
        "profile": "dictate",
        "trigger": "push_to_talk",
        "started_at": "2026-09-11T14:48:12Z",
        "finalized_at": null,
        "capture": {
            "mode": "warm", "device": "mic", "configured_pre_roll_ms": 0, "gap": "drop"
        },
        "segments": [],
        "continuations": 0,
        "audio": {
            "path": "/tmp/a.wav", "format": "wav", "sample_rate": 16000,
            "channels": 1, "bytes": 0, "duration_ms": 0
        },
        "levels": {
            "peak_dbfs": -60.0, "mean_rms_dbfs": -60.0,
            "speech_ms": 0, "silence_ms": 0, "clipped_samples": 0
        },
        "outcome": "ok"
    }"#;

    let parsed: SessionRecord = serde_json::from_str(minimal).expect("should parse");
    assert!(parsed.warnings.is_empty());
    assert!(parsed.sinks.is_empty());
    assert_eq!(parsed.transcript, None);
}
