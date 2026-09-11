//! Presenters and the event log, driven through a real daemon.
//!
//! What matters here is that turning feedback off actually stops the events, that the log on
//! disk is complete and parseable, and that a command presenter receives what it was
//! promised. All of it without a microphone.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::oneshot;
use vc_ipc::protocol::Command;
use vc_ipc::Client;

const TIMEOUT: Duration = Duration::from_secs(5);

struct Harness {
    socket: PathBuf,
    data: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    joined: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start(config: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("config.toml"), config).expect("write config");

        let socket = dir.path().join("vc.sock");
        let data = dir.path().join("data");
        let daemon =
            vc_daemon::Daemon::with_storage(dir.path(), socket.clone(), Some(data.clone()))
                .expect("daemon");

        let (tx, rx) = oneshot::channel();
        let joined = tokio::spawn(async move {
            daemon
                .run(async {
                    let _ = rx.await;
                })
                .await
                .expect("clean exit");
        });

        for _ in 0..200 {
            if vc_ipc::client::is_running(&socket, Duration::from_millis(100)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            socket,
            data,
            shutdown: Some(tx),
            joined: Some(joined),
            _dir: dir,
        }
    }

    fn send(&self, command: Command) {
        let _ = Client::connect(&self.socket, TIMEOUT)
            .expect("connect")
            .send(command);
    }

    /// The event log, parsed.
    fn log(&self) -> Vec<vc_core::Envelope> {
        let path = self.data.join("events.jsonl");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        text.lines()
            .map(|line| {
                vc_core::Envelope::from_ndjson(line)
                    .unwrap_or_else(|error| panic!("log line is not valid: {error}\n{line}"))
            })
            .collect()
    }

    /// Shut the daemon down and hand back the temp directory.
    ///
    /// Returned rather than dropped so the caller can still read what was written: dropping
    /// a `TempDir` deletes it, which makes "assert on the file after shutdown" quietly
    /// impossible.
    async fn stop(mut self) -> tempfile::TempDir {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.joined.take() {
            let _ = handle.await;
        }
        self._dir
    }
}

const BASE: &str = r#"
[defaults.capture]
mode = "on_demand"
pre_roll_ms = 0
idle_release_secs = 0
[defaults.continuation]
gap = "drop"
[profiles.dictate]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_event_log_is_written_and_every_line_parses() {
    // This file is the corpus for working out, weeks later, what the configuration should
    // have been. A single malformed line makes the whole thing awkward to analyse.
    let harness = Harness::start(BASE).await;
    harness.send(Command::Reload);
    harness.send(Command::Reload);

    let data = harness.data.clone();
    let _kept = harness.stop().await;

    let text = std::fs::read_to_string(data.join("events.jsonl")).expect("the log should exist");
    assert!(!text.trim().is_empty(), "the log is empty");
    for line in text.lines() {
        let envelope = vc_core::Envelope::from_ndjson(line)
            .unwrap_or_else(|error| panic!("unparseable line: {error}\n{line}"));
        assert_eq!(envelope.v, vc_core::EVENT_SCHEMA_VERSION);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_survives_shutdown_with_its_last_events_intact() {
    // The log is buffered. Exiting without flushing loses exactly the events that say how the
    // session ended — the ones worth having.
    let harness = Harness::start(BASE).await;
    harness.send(Command::Reload);
    let data = harness.data.clone();
    let _kept = harness.stop().await;

    let text = std::fs::read_to_string(data.join("events.jsonl")).expect("log");
    assert!(
        text.contains("config_reloaded"),
        "the last event before shutdown was lost: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn turning_feedback_off_stops_the_events_rather_than_hiding_them_later() {
    // Suppressed at the source: a user who does not want a level meter should not be paying
    // to produce twenty measurements a second for nobody.
    let harness = Harness::start(&format!("{BASE}\n[feedback]\nenabled = false\n")).await;
    harness.send(Command::Start {
        profile: "dictate".to_owned(),
    });
    harness.send(Command::Stop {
        profile: "dictate".to_owned(),
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let log = harness.log();
    let feedback: Vec<&str> = log
        .iter()
        .map(|event| event.event.kind())
        .filter(|kind| {
            matches!(
                *kind,
                "level" | "recording_started" | "pipeline_started" | "pipeline_finished"
            )
        })
        .collect();

    assert!(feedback.is_empty(), "feedback leaked through: {feedback:?}");
    let _ = harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_reach_the_log_even_with_all_feedback_off() {
    // Disabling both indicators must not become a way to hide a failure.
    let harness = Harness::start(&format!(
        "{BASE}\n[feedback]\nenabled = false\npresenters = []\n"
    ))
    .await;

    // A broken reload produces a config error, which is not feedback.
    std::fs::write(
        harness._dir.path().join("config.toml"),
        "[profiles.broken\n",
    )
    .expect("rewrite");
    harness.send(Command::Reload);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let kinds: Vec<&str> = harness.log().iter().map(|e| e.event.kind()).collect();
    assert!(
        kinds.contains(&"error"),
        "the error was suppressed: {kinds:?}"
    );
    let _ = harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_presenter_receives_each_event_as_json_on_stdin() {
    let dir = tempfile::tempdir().expect("temp dir");
    let out = dir.path().join("seen.jsonl");
    let script = dir.path().join("presenter.sh");
    std::fs::write(&script, format!("cat >> {:?}\n", out.display().to_string()))
        .expect("write script");

    let harness = Harness::start(&format!(
        r#"
{BASE}
[presenters.script]
type = "command"
cmd = ["/bin/sh", "{}"]
events = ["config_reloaded"]

[feedback]
presenters = ["socket", "script"]
"#,
        script.display()
    ))
    .await;

    harness.send(Command::Reload);
    for _ in 0..100 {
        if out.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    harness.stop().await;

    let seen = std::fs::read_to_string(&out).expect("the presenter should have been run");
    let envelope = vc_core::Envelope::from_ndjson(seen.trim()).expect("valid event JSON");
    assert_eq!(envelope.event.kind(), "config_reloaded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_presenter_allow_list_keeps_out_what_it_did_not_ask_for() {
    // One process per event is not free. A bar module that cares about start and stop should
    // not be woken twenty times a second by level updates.
    let dir = tempfile::tempdir().expect("temp dir");
    let out = dir.path().join("seen.jsonl");
    let script = dir.path().join("presenter.sh");
    std::fs::write(&script, format!("cat >> {:?}\n", out.display().to_string()))
        .expect("write script");

    let harness = Harness::start(&format!(
        r#"
{BASE}
[presenters.script]
type = "command"
cmd = ["/bin/sh", "{}"]
events = ["recording_started"]

[feedback]
presenters = ["socket", "script"]
"#,
        script.display()
    ))
    .await;

    // Reloads are not in the allow-list, so nothing should run.
    harness.send(Command::Reload);
    harness.send(Command::Reload);
    tokio::time::sleep(Duration::from_millis(300)).await;
    harness.stop().await;

    assert!(
        !out.exists(),
        "the presenter ran for an event it did not ask for"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_presenter_that_cannot_run_does_not_take_the_daemon_with_it() {
    let harness = Harness::start(&format!(
        r#"
{BASE}
[presenters.broken]
type = "command"
cmd = ["/definitely/not/a/program"]

[feedback]
presenters = ["socket", "broken"]
"#
    ))
    .await;

    harness.send(Command::Reload);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Still answering.
    match Client::connect(&harness.socket, TIMEOUT)
        .expect("connect")
        .send(Command::Ping)
    {
        Ok(vc_ipc::protocol::Response::Pong { .. }) => {}
        other => panic!("the daemon stopped answering: {other:?}"),
    }
    let _ = harness.stop().await;
}
