//! The event stream is a published contract. These tests pin its wire format.
//!
//! They are deliberately written as literal JSON rather than round-trip assertions. A
//! round-trip test passes happily while a field is renamed underneath it — and every
//! consumer outside this repository breaks. If one of these fails, the question is not "how
//! do I update the expected string" but "does this need a schema version bump and a note in
//! docs/events.md".

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the shared helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used)]

use time::macros::datetime;
use vc_core::event::{DeviceCloseReason, Envelope, Event, PlannedSink, Stage};
use vc_core::session::{InputWarningKind, Outcome, SessionId, SinkOutcome, SkipReason, StopReason};

fn session() -> SessionId {
    SessionId::from_raw("20260911T144812Z-dictate-2hc8b")
}

/// Serialize an event as it would appear on the wire.
fn wire(event: Event) -> String {
    Envelope::for_session(
        datetime!(2026-09-11 14:48:12 UTC),
        session(),
        "dictate",
        event,
    )
    .to_ndjson()
    .expect("events must always serialize")
}

fn assert_wire(event: Event, expected: &str) {
    let actual = wire(event);
    assert_eq!(
        actual, expected,
        "\nthe event wire format changed.\n  was: {expected}\n  now: {actual}\n\
         If this is intentional, bump EVENT_SCHEMA_VERSION and update docs/events.md."
    );
}

const PREFIX: &str = r#"{"v":1,"ts":"2026-09-11T14:48:12Z","session":"20260911T144812Z-dictate-2hc8b","profile":"dictate""#;

// ── the envelope ─────────────────────────────────────────────────────────────

#[test]
fn daemon_events_carry_no_session_or_profile() {
    let line = Envelope::for_daemon(
        datetime!(2026-09-11 14:48:12 UTC),
        Event::ConfigReloaded { warnings: 2 },
    )
    .to_ndjson()
    .expect("serializes");

    // Absent rather than null: a consumer checks for the key, and `null` would force every
    // one of them to handle a third case.
    assert_eq!(
        line,
        r#"{"v":1,"ts":"2026-09-11T14:48:12Z","event":"config_reloaded","warnings":2}"#
    );
}

#[test]
fn the_event_kind_is_a_flat_field_not_a_nested_object() {
    // `jq 'select(.event == "level")'` should work without reaching into a wrapper.
    let line = wire(Event::CooldownStarted { ms: 1500 });
    assert!(line.contains(r#""event":"cooldown_started""#), "{line}");
    assert!(line.contains(r#""ms":1500"#), "{line}");
}

#[test]
fn every_event_round_trips_through_json() {
    for event in all_events() {
        let kind = event.kind();
        let line = wire(event.clone());
        let parsed = Envelope::from_ndjson(&line)
            .unwrap_or_else(|error| panic!("{kind} failed to parse back: {error}\n{line}"));
        assert_eq!(parsed.event, event, "{kind} did not survive a round trip");
        assert_eq!(parsed.v, 1);
    }
}

#[test]
fn every_event_reports_the_kind_it_serializes_as() {
    // `Event::kind` is what presenter filters match against, so a mismatch would silently
    // drop events for anyone using an allow-list.
    for event in all_events() {
        let line = wire(event.clone());
        let expected = format!(r#""event":"{}""#, event.kind());
        assert!(
            line.contains(&expected),
            "{} serializes without {expected}: {line}",
            event.kind()
        );
    }
}

// ── the listening indicator's events ─────────────────────────────────────────

#[test]
fn recording_started_reports_the_pre_roll_actually_captured() {
    assert_wire(
        Event::RecordingStarted {
            segment: 0,
            pre_roll_ms: 500,
            device: "Wireless Microphone RX".to_owned(),
        },
        &format!(
            r#"{PREFIX},"event":"recording_started","segment":0,"pre_roll_ms":500,"device":"Wireless Microphone RX"}}"#
        ),
    );
}

#[test]
fn level_carries_everything_a_meter_needs() {
    assert_wire(
        Event::Level {
            rms_dbfs: -32.5,
            peak_dbfs: -12.0,
            speech: true,
            clipping: false,
        },
        &format!(
            r#"{PREFIX},"event":"level","rms_dbfs":-32.5,"peak_dbfs":-12.0,"speech":true,"clipping":false}}"#
        ),
    );
}

#[test]
fn an_input_warning_names_a_cause_the_user_can_act_on() {
    assert_wire(
        Event::InputWarning {
            kind: InputWarningKind::DeviceMuted,
            detail: Some("source 47 is muted".to_owned()),
        },
        &format!(
            r#"{PREFIX},"event":"input_warning","kind":"device_muted","detail":"source 47 is muted"}}"#
        ),
    );
}

#[test]
fn an_input_warning_omits_detail_when_there_is_none() {
    assert_wire(
        Event::InputWarning {
            kind: InputWarningKind::Silence,
            detail: None,
        },
        &format!(r#"{PREFIX},"event":"input_warning","kind":"silence"}}"#),
    );
}

#[test]
fn recording_stopped_distinguishes_a_release_from_the_watchdog() {
    // An indicator should show these differently: one is normal, the other means a release
    // keybind never fired and the user needs to fix their config.
    assert_wire(
        Event::RecordingStopped {
            segment: 0,
            duration_ms: 2400,
            reason: StopReason::Watchdog,
        },
        &format!(
            r#"{PREFIX},"event":"recording_stopped","segment":0,"duration_ms":2400,"reason":"watchdog"}}"#
        ),
    );
}

#[test]
fn a_continuation_reports_how_close_to_the_deadline_it_was() {
    assert_wire(
        Event::RecordingResumed {
            segment: 1,
            resumed_after_ms: 1420,
        },
        &format!(r#"{PREFIX},"event":"recording_resumed","segment":1,"resumed_after_ms":1420}}"#),
    );
}

// ── the processing indicator's events ────────────────────────────────────────

#[test]
fn pipeline_started_announces_the_whole_plan_before_anything_runs() {
    // The entire point of this event: a progress panel has to draw every row, greyed out,
    // before the first callback starts. Learning about sinks one at a time would give a list
    // that grows while the user watches it.
    assert_wire(
        Event::PipelineStarted {
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
        &format!(
            r#"{PREFIX},"event":"pipeline_started","transcriber":"openai","sinks":[{{"id":0,"name":"agent","kind":"command","requires_text":true}},{{"id":1,"name":"archive","kind":"command","requires_text":false}}]}}"#
        ),
    );
}

#[test]
fn pipeline_started_omits_the_transcriber_when_a_profile_has_none() {
    assert_wire(
        Event::PipelineStarted {
            transcriber: None,
            sinks: vec![],
        },
        &format!(r#"{PREFIX},"event":"pipeline_started","sinks":[]}}"#),
    );
}

#[test]
fn a_successful_sink_is_a_tick() {
    assert_wire(
        Event::SinkFinished {
            id: 0,
            name: "agent".to_owned(),
            outcome: SinkOutcome::Ok,
            latency_ms: 812,
            attempts: 1,
        },
        &format!(
            r#"{PREFIX},"event":"sink_finished","id":0,"name":"agent","outcome":{{"status":"ok"}},"latency_ms":812,"attempts":1}}"#
        ),
    );
}

#[test]
fn a_failed_sink_carries_the_reason_it_failed() {
    assert_wire(
        Event::SinkFinished {
            id: 1,
            name: "webhook".to_owned(),
            outcome: SinkOutcome::Failed {
                error: "connection refused".to_owned(),
            },
            latency_ms: 30_000,
            attempts: 3,
        },
        &format!(
            r#"{PREFIX},"event":"sink_finished","id":1,"name":"webhook","outcome":{{"status":"failed","error":"connection refused"}},"latency_ms":30000,"attempts":3}}"#
        ),
    );
}

#[test]
fn a_skipped_sink_says_why_it_was_skipped() {
    // Otherwise a greyed-out row is indistinguishable from one that silently never ran.
    assert_wire(
        Event::SinkFinished {
            id: 2,
            name: "clipboard".to_owned(),
            outcome: SinkOutcome::Skipped {
                reason: SkipReason::NoTranscript,
            },
            latency_ms: 0,
            attempts: 0,
        },
        &format!(
            r#"{PREFIX},"event":"sink_finished","id":2,"name":"clipboard","outcome":{{"status":"skipped","reason":"no_transcript"}},"latency_ms":0,"attempts":0}}"#
        ),
    );
}

#[test]
fn pipeline_finished_summarises_without_needing_the_earlier_events() {
    // A consumer that connected late should still be able to render a final state.
    assert_wire(
        Event::PipelineFinished {
            ok: 2,
            failed: 1,
            skipped: 0,
            total_ms: 31_200,
            outcome: Outcome::Partial,
        },
        &format!(
            r#"{PREFIX},"event":"pipeline_finished","ok":2,"failed":1,"skipped":0,"total_ms":31200,"outcome":"partial"}}"#
        ),
    );
}

// ── feedback routing ─────────────────────────────────────────────────────────

#[test]
fn each_event_belongs_to_at_most_one_indicator() {
    // `feedback.listening` and `feedback.processing` are independent switches, so an event
    // claimed by both would be impossible to turn off.
    for event in all_events() {
        assert!(
            !(event.is_listening_feedback() && event.is_processing_feedback()),
            "{} is claimed by both indicators",
            event.kind()
        );
    }
}

#[test]
fn the_listening_indicator_gets_what_it_needs_and_no_levels_leak_into_processing() {
    assert!(Event::Level {
        rms_dbfs: -30.0,
        peak_dbfs: -10.0,
        speech: true,
        clipping: false
    }
    .is_listening_feedback());

    assert!(Event::PipelineStarted {
        transcriber: None,
        sinks: vec![]
    }
    .is_processing_feedback());

    // Errors and daemon lifecycle belong to neither, so disabling both indicators must not
    // hide a failure.
    let error = Event::Error {
        stage: Stage::Capture,
        message: "no such device".to_owned(),
    };
    assert!(!error.is_listening_feedback() && !error.is_processing_feedback());
}

/// One of every variant, so the exhaustiveness tests above cannot silently miss a new one.
fn all_events() -> Vec<Event> {
    vec![
        Event::DaemonReady {
            version: "0.1.0".to_owned(),
            socket: "/run/user/1000/voice-commander.sock".into(),
        },
        Event::ConfigReloaded { warnings: 0 },
        Event::DeviceOpened {
            device: "Wireless Microphone RX".to_owned(),
            sample_rate: 48_000,
            channels: 1,
        },
        Event::DeviceClosed {
            reason: DeviceCloseReason::Idle,
        },
        Event::RecordingStarted {
            segment: 0,
            pre_roll_ms: 500,
            device: "mic".to_owned(),
        },
        Event::Level {
            rms_dbfs: -32.5,
            peak_dbfs: -12.0,
            speech: true,
            clipping: false,
        },
        Event::InputWarning {
            kind: InputWarningKind::TooQuiet,
            detail: None,
        },
        Event::RecordingStopped {
            segment: 0,
            duration_ms: 2400,
            reason: StopReason::Released,
        },
        Event::CooldownStarted { ms: 1500 },
        Event::RecordingResumed {
            segment: 1,
            resumed_after_ms: 900,
        },
        Event::SessionCancelled {
            reason: "user".to_owned(),
        },
        Event::SessionFinalized {
            audio_path: "/tmp/audio.wav".into(),
            total_ms: 4200,
            segments: 2,
        },
        Event::PipelineStarted {
            transcriber: Some("openai".to_owned()),
            sinks: vec![PlannedSink {
                id: 0,
                name: "agent".to_owned(),
                kind: "command".to_owned(),
                requires_text: true,
            }],
        },
        Event::TranscribeStarted {
            transcriber: "openai".to_owned(),
        },
        Event::TranscribeDone {
            chars: 142,
            latency_ms: 900,
            language: Some("en".to_owned()),
        },
        Event::TranscribeFailed {
            error: "401".to_owned(),
            latency_ms: 120,
        },
        Event::SinkStarted {
            id: 0,
            name: "agent".to_owned(),
        },
        Event::SinkFinished {
            id: 0,
            name: "agent".to_owned(),
            outcome: SinkOutcome::Ok,
            latency_ms: 812,
            attempts: 1,
        },
        Event::PipelineFinished {
            ok: 1,
            failed: 0,
            skipped: 0,
            total_ms: 1700,
            outcome: Outcome::Ok,
        },
        Event::Error {
            stage: Stage::Sink,
            message: "boom".to_owned(),
        },
    ]
}
