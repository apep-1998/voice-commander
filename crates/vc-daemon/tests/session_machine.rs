//! The session state machine, tested to the millisecond.
//!
//! Every timestamp here is supplied rather than read from a clock, so the cases that matter
//! — resuming one millisecond inside the continuation window, or one millisecond outside —
//! are exercised exactly, without a single `sleep` and without flaking on a loaded machine.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use time::macros::datetime;
use time::OffsetDateTime;
use vc_audio::level::LevelMeter;
use vc_core::config::{CaptureMode, Config, GapMode, Layer, LevelsConfig, Profile};
use vc_core::session::StopReason;
use vc_daemon::recorder::{Phase, Recorder, Transition};

const RATE: u32 = 16_000;

fn levels() -> LevelsConfig {
    LevelsConfig {
        silence_dbfs: -55.0,
        too_quiet_dbfs: -40.0,
        speech_dbfs: -45.0,
        silence_warn_after_ms: 1_000,
    }
}

/// Build a profile from TOML, so these tests exercise the same resolution path the daemon
/// uses rather than constructing the struct by hand.
fn profile(overrides: &str) -> Profile {
    let text = format!("[profiles.t]\n{overrides}");
    Config::from_layers(&[
        Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        Layer::new("<test>", &text),
    ])
    .expect("valid configuration")
    .config
    .profiles
    .remove("t")
    .expect("profile t")
}

fn recorder(overrides: &str) -> Recorder {
    Recorder::new(&profile(overrides), &levels(), RATE, "test-mic".to_owned())
}

/// A distinguishable tone, so audio from different moments can be told apart in the output.
fn tone(value: f32, ms: u64) -> Vec<f32> {
    vec![value; (RATE as u64 * ms / 1000) as usize]
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn wall() -> OffsetDateTime {
    datetime!(2026-09-11 14:48:12 UTC)
}

/// Push audio as a real capture loop would, with the level meter it would use.
fn feed(recorder: &mut Recorder, samples: &[f32]) {
    let snapshot = LevelMeter::new(levels()).measure(samples);
    recorder.push(samples, &snapshot);
}

fn duration_ms(samples: usize) -> u64 {
    samples as u64 * 1000 / u64::from(RATE)
}

// ── the ordinary case ────────────────────────────────────────────────────────

#[test]
fn a_press_and_release_produces_one_segment() {
    let mut recorder = recorder("");
    let t0 = Instant::now();

    assert!(matches!(
        recorder.start(t0, wall(), Vec::new(), 0),
        Transition::Started { segment: 0, .. }
    ));
    assert_eq!(recorder.phase(), Phase::Recording);

    feed(&mut recorder, &tone(0.5, 1_000));

    assert!(matches!(
        recorder.stop(t0 + ms(1_000), wall(), StopReason::Released),
        Transition::Stopped { segment: 0, .. }
    ));
    assert_eq!(recorder.phase(), Phase::Cooling);

    // The window has to expire before anything downstream runs.
    assert_eq!(recorder.tick(t0 + ms(1_100), wall()), Transition::Nothing);
    assert_eq!(recorder.tick(t0 + ms(2_600), wall()), Transition::Finalized);

    let finished = recorder.finish();
    assert_eq!(finished.segments.len(), 1);
    assert_eq!(finished.continuations, 0);
    assert_eq!(duration_ms(finished.audio.len()), 1_000);
}

#[test]
fn releasing_the_key_does_not_finalize_the_session() {
    // The single most important property here. Everything about continuation depends on it.
    let mut recorder = recorder("");
    let t0 = Instant::now();
    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500));
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);

    assert_eq!(
        recorder.phase(),
        Phase::Cooling,
        "release must open the window"
    );
}

#[test]
fn stopping_twice_is_not_an_error() {
    // A compositor can deliver a release without a matching press. A keybind that errors
    // when the user did nothing wrong is worse than one that quietly does nothing.
    let mut recorder = recorder("");
    let t0 = Instant::now();
    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(100), wall(), StopReason::Released);

    assert_eq!(
        recorder.stop(t0 + ms(200), wall(), StopReason::Released),
        Transition::Nothing
    );
}

#[test]
fn starting_twice_is_not_an_error() {
    // Key repeat, or a compositor delivering the press twice.
    let mut recorder = recorder("");
    let t0 = Instant::now();
    recorder.start(t0, wall(), Vec::new(), 0);

    assert_eq!(
        recorder.start(t0 + ms(10), wall(), tone(0.9, 100), 0),
        Transition::Nothing
    );
    assert_eq!(recorder.segment_index(), 0);
}

// ── pre-roll ─────────────────────────────────────────────────────────────────

#[test]
fn pre_roll_audio_lands_at_the_start_of_the_recording() {
    // The whole point of the pre-roll buffer: the syllable spoken before the key registered
    // has to come first, not be appended somewhere.
    let mut recorder = recorder("capture = { mode = \"preroll\", pre_roll_ms = 500 }");
    let t0 = Instant::now();

    let lead_in = tone(0.25, 500);
    recorder.start(t0, wall(), lead_in.clone(), 320);
    feed(&mut recorder, &tone(0.75, 1_000));
    recorder.stop(t0 + ms(1_000), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    let finished = recorder.finish();
    assert_eq!(duration_ms(finished.audio.len()), 1_500);
    assert_eq!(
        &finished.audio[..lead_in.len()],
        &lead_in[..],
        "the pre-roll must be at the front"
    );
}

#[test]
fn the_pre_roll_actually_captured_is_recorded_not_the_configured_amount() {
    // Right after the device opens, less lookback exists than was configured. Reporting the
    // configured figure would make the tuning data useless.
    let mut recorder = recorder("capture = { mode = \"preroll\", pre_roll_ms = 500 }");
    let t0 = Instant::now();

    match recorder.start(t0, wall(), tone(0.25, 120), 0) {
        Transition::Started { pre_roll_ms, .. } => assert_eq!(pre_roll_ms, 120),
        other => panic!("expected a start, got {other:?}"),
    }
}

#[test]
fn speech_found_in_the_pre_roll_window_is_recorded_for_tuning() {
    // This is the number that says whether pre_roll_ms is long enough: consistently near the
    // configured value means speech is still being clipped.
    let mut recorder = recorder("capture = { mode = \"preroll\", pre_roll_ms = 500 }");
    let t0 = Instant::now();
    recorder.start(t0, wall(), tone(0.25, 500), 310);
    recorder.stop(t0 + ms(100), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    assert_eq!(recorder.finish().segments[0].speech_in_pre_roll_ms, 310);
}

#[test]
fn warm_mode_starts_at_the_keypress_with_no_lookback() {
    let mut recorder = recorder("capture = { mode = \"warm\", pre_roll_ms = 0 }");
    let t0 = Instant::now();

    match recorder.start(t0, wall(), Vec::new(), 0) {
        Transition::Started { pre_roll_ms, .. } => assert_eq!(pre_roll_ms, 0),
        other => panic!("expected a start, got {other:?}"),
    }
}

// ── continuation: the cooldown window ────────────────────────────────────────

#[test]
fn pressing_again_inside_the_window_continues_the_same_session() {
    // "I said my piece, let go, and immediately remembered one more thing."
    let mut recorder = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 800));
    recorder.stop(t0 + ms(800), wall(), StopReason::Released);

    match recorder.start(t0 + ms(1_600), wall(), Vec::new(), 0) {
        Transition::Resumed {
            segment,
            resumed_after_ms,
        } => {
            assert_eq!(segment, 1);
            assert_eq!(resumed_after_ms, 800);
        }
        other => panic!("expected a resume, got {other:?}"),
    }

    feed(&mut recorder, &tone(0.5, 400));
    recorder.stop(t0 + ms(2_000), wall(), StopReason::Released);
    recorder.tick(t0 + ms(4_000), wall());

    let finished = recorder.finish();
    assert_eq!(finished.segments.len(), 2, "one session, two segments");
    assert_eq!(finished.continuations, 1);
}

#[test]
fn pressing_again_after_the_window_is_a_new_session() {
    let mut recorder = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    assert_eq!(recorder.tick(t0 + ms(2_100), wall()), Transition::Finalized);
    assert_eq!(recorder.phase(), Phase::Idle);
}

#[test]
fn the_window_boundary_is_exact() {
    // One millisecond either side of the deadline must behave differently, and this is
    // impossible to test reliably against a real clock.
    let cooldown = 1_500;

    let mut just_inside = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();
    just_inside.start(t0, wall(), Vec::new(), 0);
    just_inside.stop(t0 + ms(100), wall(), StopReason::Released);
    assert!(
        matches!(
            just_inside.start(t0 + ms(100 + cooldown - 1), wall(), Vec::new(), 0),
            Transition::Resumed { .. }
        ),
        "1ms before the deadline must still continue the session"
    );

    let mut just_outside = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();
    just_outside.start(t0, wall(), Vec::new(), 0);
    just_outside.stop(t0 + ms(100), wall(), StopReason::Released);
    assert_eq!(
        just_outside.tick(t0 + ms(100 + cooldown), wall()),
        Transition::Finalized,
        "at the deadline the session must be over"
    );
}

#[test]
fn how_close_to_the_deadline_the_user_was_is_recorded() {
    // The other half of the tuning story: if these cluster just under cooldown_ms, the
    // window is too short and continuations are being missed.
    let mut recorder = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    recorder.start(t0 + ms(1_920), wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(2_000), wall(), StopReason::Released);
    recorder.tick(t0 + ms(4_000), wall());

    let finished = recorder.finish();
    assert_eq!(
        finished.segments[0].resumed_after_ms, None,
        "the first segment resumed nothing"
    );
    assert_eq!(finished.segments[1].resumed_after_ms, Some(1_420));
}

#[test]
fn a_zero_cooldown_finalizes_immediately() {
    // Opting out of continuation entirely must not leave the session hanging.
    let mut recorder = recorder("session = { cooldown_ms = 0 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);

    assert_eq!(recorder.tick(t0 + ms(500), wall()), Transition::Finalized);
}

// ── continuation: what happens to the gap ────────────────────────────────────

#[test]
fn keeping_the_gap_produces_one_continuous_take() {
    // The device stayed open, so the pause is just the middle of the user's sentence.
    let mut recorder = recorder(
        "capture = { mode = \"preroll\" }\ncontinuation = { gap = \"keep\" }\nsession = { cooldown_ms = 1500 }",
    );
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500)); // spoken
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    feed(&mut recorder, &tone(0.2, 300)); // the pause, still captured
    recorder.start(t0 + ms(800), wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 400)); // spoken again
    recorder.stop(t0 + ms(1_200), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    let finished = recorder.finish();
    assert_eq!(
        duration_ms(finished.audio.len()),
        1_200,
        "500 + 300 of pause + 400"
    );
    // The pause is there, in the middle, at its own level.
    let middle = finished.audio[RATE as usize * 600 / 1000];
    assert!(
        (middle - 0.2).abs() < 1e-6,
        "the gap audio is missing: {middle}"
    );
}

#[test]
fn dropping_the_gap_splices_the_segments_together() {
    let mut recorder =
        recorder("continuation = { gap = \"drop\" }\nsession = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500));
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    feed(&mut recorder, &tone(0.2, 300)); // captured, but discarded
    recorder.start(t0 + ms(800), wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.7, 400));
    recorder.stop(t0 + ms(1_200), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    let finished = recorder.finish();
    assert_eq!(
        duration_ms(finished.audio.len()),
        900,
        "500 + 400, no pause"
    );
    // The splice is exactly where the second segment begins.
    assert_eq!(finished.segments[1].start_offset_ms, 500);
}

#[test]
fn inserting_silence_separates_the_utterances() {
    let mut recorder = recorder(
        "continuation = { gap = \"silence\", silence_ms = 300 }\nsession = { cooldown_ms = 1500 }",
    );
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500));
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    recorder.start(t0 + ms(800), wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.7, 400));
    recorder.stop(t0 + ms(1_200), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    let finished = recorder.finish();
    assert_eq!(duration_ms(finished.audio.len()), 1_200, "500 + 300 + 400");
    let inserted = finished.audio[RATE as usize * 600 / 1000];
    assert_eq!(inserted, 0.0, "expected digital silence, got {inserted}");
}

#[test]
fn gap_audio_is_discarded_when_the_session_simply_ends() {
    // The gap is only part of the recording if the user came back. Otherwise it is a second
    // and a half of them having already stopped talking.
    let mut recorder =
        recorder("continuation = { gap = \"keep\" }\nsession = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500));
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    feed(&mut recorder, &tone(0.2, 1_500)); // the whole window
    recorder.tick(t0 + ms(2_100), wall());

    assert_eq!(
        duration_ms(recorder.finish().audio.len()),
        500,
        "trailing silence must not be kept"
    );
}

#[test]
fn on_demand_mode_cannot_keep_the_gap_and_says_so() {
    // The device is shut during the gap, so there is nothing to keep. Silently doing
    // something else would leave the user unable to explain the jump in their recording.
    let mut recorder = recorder(
        "capture = { mode = \"on_demand\", pre_roll_ms = 0, idle_release_secs = 0 }\ncontinuation = { gap = \"keep\" }",
    );
    let t0 = Instant::now();
    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(100), wall(), StopReason::Released);
    recorder.tick(t0 + ms(2_000), wall());

    let finished = recorder.finish();
    assert_eq!(finished.capture.gap, GapMode::Drop);
    assert_eq!(finished.capture.gap_downgraded_to, Some(GapMode::Drop));
    assert_eq!(finished.capture.mode, CaptureMode::OnDemand);
}

// ── limits ───────────────────────────────────────────────────────────────────

#[test]
fn the_watchdog_stops_a_recording_whose_release_never_arrived() {
    // Release the modifier before the key and the compositor never fires the release.
    // Without this, the daemon records until the disk fills.
    let mut recorder = recorder("session = { max_recording_secs = 2, max_total_secs = 10 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    assert_eq!(recorder.tick(t0 + ms(1_999), wall()), Transition::Nothing);

    match recorder.tick(t0 + ms(2_000), wall()) {
        Transition::Stopped { reason, .. } => assert_eq!(reason, StopReason::Watchdog),
        other => panic!("the watchdog did not fire: {other:?}"),
    }
}

#[test]
fn a_watchdog_stop_is_recorded_so_the_user_can_be_told() {
    let mut recorder = recorder("session = { max_recording_secs = 1, max_total_secs = 10 }");
    let t0 = Instant::now();
    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.tick(t0 + ms(1_000), wall());
    recorder.tick(t0 + ms(5_000), wall());

    assert_eq!(
        recorder.finish().segments[0].stop_reason,
        StopReason::Watchdog
    );
}

#[test]
fn the_total_limit_ends_a_session_that_kept_being_continued() {
    // The limit that catches a session extended one continuation at a time, where no single
    // segment ever comes near the per-recording watchdog. Reaching it and then opening
    // another continuation window would make the limit meaningless.
    let mut recorder = recorder(
        "session = { max_recording_secs = 2, max_total_secs = 2, cooldown_ms = 1500 }\n\
         continuation = { gap = \"drop\", max_segments = 5 }",
    );
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 1_500));
    recorder.stop(t0 + ms(1_500), wall(), StopReason::Released);

    recorder.start(t0 + ms(2_000), wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 600)); // total is now 2100ms, over the limit

    match recorder.tick(t0 + ms(2_100), wall()) {
        Transition::Stopped {
            reason,
            cooldown_ms,
            ..
        } => {
            // Not the watchdog: this segment is only 100ms old.
            assert_eq!(reason, StopReason::TotalLimit);
            assert_eq!(cooldown_ms, 0, "no window after a limit was hit");
        }
        other => panic!("expected a stop, got {other:?}"),
    }
    assert_eq!(recorder.tick(t0 + ms(2_101), wall()), Transition::Finalized);
}

#[test]
fn a_session_stops_continuing_once_it_hits_the_segment_limit() {
    let mut recorder =
        recorder("continuation = { max_segments = 2 }\nsession = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    recorder.stop(t0 + ms(100), wall(), StopReason::Released);
    recorder.start(t0 + ms(200), wall(), Vec::new(), 0);

    match recorder.stop(t0 + ms(300), wall(), StopReason::Released) {
        Transition::Stopped { cooldown_ms, .. } => {
            assert_eq!(cooldown_ms, 0, "the second segment is the last one allowed");
        }
        other => panic!("expected a stop, got {other:?}"),
    }
    assert_eq!(recorder.tick(t0 + ms(301), wall()), Transition::Finalized);
}

// ── cancel ───────────────────────────────────────────────────────────────────

#[test]
fn cancelling_discards_everything() {
    let mut recorder = recorder("");
    let t0 = Instant::now();

    recorder.start(t0, wall(), tone(0.3, 200), 0);
    feed(&mut recorder, &tone(0.5, 1_000));
    assert_eq!(recorder.cancel(), Transition::Cancelled);

    assert_eq!(recorder.phase(), Phase::Idle);
    let finished = recorder.finish();
    assert!(
        finished.audio.is_empty(),
        "cancelled audio must not survive"
    );
    assert!(finished.segments.is_empty());
}

#[test]
fn cancelling_during_the_cooldown_window_also_works() {
    // The escape hatch has to work in the state where the user is most likely to reach for
    // it — they have just let go and realised they said the wrong thing.
    let mut recorder = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500));
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);

    assert_eq!(recorder.cancel(), Transition::Cancelled);
    assert_eq!(recorder.phase(), Phase::Idle);
    assert!(recorder.finish().audio.is_empty());
}

#[test]
fn cancelling_when_nothing_is_happening_does_nothing() {
    assert_eq!(recorder("").cancel(), Transition::Nothing);
}

// ── bookkeeping ──────────────────────────────────────────────────────────────

#[test]
fn segment_offsets_locate_each_segment_in_the_finished_audio() {
    // `stats` and any future editor need to map a segment back to where it lives.
    let mut recorder =
        recorder("continuation = { gap = \"drop\" }\nsession = { cooldown_ms = 2000 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 700));
    recorder.stop(t0 + ms(700), wall(), StopReason::Released);
    recorder.start(t0 + ms(1_000), wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 300));
    recorder.stop(t0 + ms(1_300), wall(), StopReason::Released);
    recorder.tick(t0 + ms(4_000), wall());

    let finished = recorder.finish();
    assert_eq!(finished.segments[0].start_offset_ms, 0);
    assert_eq!(finished.segments[0].duration_ms, 700);
    assert_eq!(finished.segments[1].start_offset_ms, 700);
    assert_eq!(finished.segments[1].duration_ms, 300);
    assert_eq!(
        finished.segments.iter().map(|s| s.duration_ms).sum::<u64>(),
        duration_ms(finished.audio.len())
    );
}

#[test]
fn kept_gap_audio_belongs_to_the_file_but_to_no_segment() {
    // These are genuinely different quantities, and conflating them makes the recorded
    // duration disagree with the file it describes.
    let mut recorder = recorder(
        "capture = { mode = \"preroll\" }\ncontinuation = { gap = \"keep\" }\nsession = { cooldown_ms = 1500 }",
    );
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 500));
    recorder.stop(t0 + ms(500), wall(), StopReason::Released);
    feed(&mut recorder, &tone(0.2, 300)); // the pause
    recorder.start(t0 + ms(800), wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 400));
    recorder.stop(t0 + ms(1_200), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    let finished = recorder.finish();
    let segment_total: u64 = finished.segments.iter().map(|s| s.duration_ms).sum();
    assert_eq!(
        segment_total, 900,
        "the segments cover only what was spoken"
    );
    assert_eq!(
        duration_ms(finished.audio.len()),
        1_200,
        "the file also holds the pause"
    );
}

#[test]
fn the_session_start_time_accounts_for_the_pre_roll() {
    // The recording genuinely begins before the keypress, and the timestamp has to say so or
    // the audio and the metadata disagree.
    let mut recorder = recorder("capture = { mode = \"preroll\", pre_roll_ms = 500 }");
    let t0 = Instant::now();
    let pressed = wall();

    recorder.start(t0, pressed, tone(0.2, 500), 0);
    recorder.stop(t0 + ms(100), pressed, StopReason::Released);
    recorder.tick(t0 + ms(3_000), pressed);

    let finished = recorder.finish();
    assert_eq!(finished.started_at, pressed - Duration::from_millis(500));
    assert_eq!(finished.segments[0].key_down_at, pressed);
}

#[test]
fn level_totals_cover_the_audio_that_was_kept() {
    let mut recorder = recorder("");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 1_000));
    recorder.stop(t0 + ms(1_000), wall(), StopReason::Released);
    recorder.tick(t0 + ms(3_000), wall());

    let levels = recorder.finish().levels;
    assert!(levels.speech_ms > 0, "a loud tone should count as speech");
    assert!(levels.peak_dbfs > -10.0, "peak was {}", levels.peak_dbfs);
    assert_eq!(levels.clipped_samples, 0);
}

#[test]
fn reported_progress_tracks_the_recording() {
    let mut recorder = recorder("session = { cooldown_ms = 1500 }");
    let t0 = Instant::now();

    recorder.start(t0, wall(), Vec::new(), 0);
    feed(&mut recorder, &tone(0.5, 750));
    assert_eq!(recorder.recorded_ms(), 750);
    assert_eq!(recorder.segment_elapsed_ms(t0 + ms(750)), 750);

    recorder.stop(t0 + ms(750), wall(), StopReason::Released);
    assert_eq!(recorder.cooldown_remaining_ms(t0 + ms(750)), 1_500);
    assert_eq!(recorder.cooldown_remaining_ms(t0 + ms(1_500)), 750);
    assert_eq!(recorder.cooldown_remaining_ms(t0 + ms(2_250)), 0);
}

#[test]
fn audio_arriving_while_idle_is_ignored() {
    // The device stays open between recordings in warm and preroll modes, so samples keep
    // arriving. None of them belong to a session.
    let mut recorder = recorder("");
    feed(&mut recorder, &tone(0.9, 5_000));

    assert_eq!(recorder.recorded_ms(), 0);
}
