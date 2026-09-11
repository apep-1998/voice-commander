//! A recording, from a socket command to files on disk.
//!
//! These drive the real daemon over its real socket and then inspect what landed in the
//! recordings directory — the same path a Hyprland keybind takes, minus the microphone.
//! Anything needing actual audio hardware is covered by `mic-test` and the `#[ignore]`d
//! tests in `vc-audio`.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the harness below is plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::oneshot;
use vc_core::session::SessionRecord;
use vc_ipc::protocol::{ActivityState, Command, Response};
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

    fn send(&self, command: Command) -> Response {
        Client::connect(&self.socket, TIMEOUT)
            .expect("connect")
            .send(command)
            .expect("command")
    }

    fn status(&self) -> ActivityState {
        match self.send(Command::Status) {
            Response::Status(status) => status.activity,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    /// Every `session.json` written so far.
    fn sessions(&self) -> Vec<SessionRecord> {
        let mut found = Vec::new();
        collect(&self.data, &mut found);
        found.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        found
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.joined.take() {
            let _ = handle.await;
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<SessionRecord>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.file_name().is_some_and(|name| name == "session.json") {
            let text = std::fs::read_to_string(&path).expect("read session.json");
            out.push(serde_json::from_str(&text).expect("parse session.json"));
        }
    }
}

/// Wait for a condition, polling rather than sleeping a fixed amount.
async fn until(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..300 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

const CONFIG: &str = r#"
[defaults.capture]
mode = "on_demand"
pre_roll_ms = 0
idle_release_secs = 0
[defaults.session]
cooldown_ms = 200
[defaults.continuation]
gap = "drop"
[profiles.dictate]
[profiles.other]
"#;

#[tokio::test(flavor = "multi_thread")]
async fn status_follows_the_recording_through_its_states() {
    // Without a device this cannot reach `Recording`, but the cooldown and idle states are
    // driven entirely by the state machine and must be observable over the socket — that is
    // what an indicator polls.
    let harness = Harness::start(CONFIG).await;
    assert_eq!(harness.status(), ActivityState::Idle);

    harness.send(Command::Start {
        profile: "dictate".to_owned(),
    });
    harness.send(Command::Stop {
        profile: "dictate".to_owned(),
    });
    harness.send(Command::Cancel);

    assert!(
        until(|| harness.status() == ActivityState::Idle).await,
        "the daemon never returned to idle"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_with_no_audio_does_not_leave_an_empty_recording() {
    // On a machine with no working microphone — a CI runner, or a user whose device is
    // muted — the daemon must not litter the recordings directory with zero-length files.
    let harness = Harness::start(CONFIG).await;

    harness.send(Command::Start {
        profile: "dictate".to_owned(),
    });
    harness.send(Command::Stop {
        profile: "dictate".to_owned(),
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    for session in harness.sessions() {
        assert!(
            session.audio.duration_ms > 0,
            "an empty session was written: {session:?}"
        );
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_without_a_start_is_harmless() {
    // A compositor can deliver a release without a press — and does, if the daemon restarts
    // while a key is held.
    let harness = Harness::start(CONFIG).await;

    match harness.send(Command::Stop {
        profile: "dictate".to_owned(),
    }) {
        Response::Accepted { .. } => {}
        other => panic!("a stray release should be accepted, got {other:?}"),
    }
    assert_eq!(harness.status(), ActivityState::Idle);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_with_nothing_in_flight_is_harmless() {
    // This is bound to Escape, so it gets pressed when nothing is happening.
    let harness = Harness::start(CONFIG).await;
    match harness.send(Command::Cancel) {
        Response::Accepted { .. } => {}
        other => panic!("expected acceptance, got {other:?}"),
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_event_stream_reports_what_the_daemon_is_doing() {
    // The contract a future indicator is built against, exercised over the real socket.
    let harness = Harness::start(CONFIG).await;
    let socket = harness.socket.clone();

    let collector = tokio::task::spawn_blocking(move || {
        let client = Client::connect(&socket, TIMEOUT).expect("connect");
        let stream = client.subscribe(vec![]).expect("subscribe");
        stream
            .take(4)
            .filter_map(Result::ok)
            .collect::<Vec<String>>()
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.send(Command::Reload);
    harness.send(Command::Reload);
    harness.send(Command::Reload);
    harness.send(Command::Reload);

    let lines = tokio::time::timeout(Duration::from_secs(5), collector)
        .await
        .map(|joined| joined.expect("collector"))
        .unwrap_or_default();

    assert!(!lines.is_empty(), "no events reached the subscriber");
    for line in &lines {
        let envelope = vc_core::Envelope::from_ndjson(line)
            .unwrap_or_else(|error| panic!("event is not valid: {error}\n{line}"));
        assert_eq!(envelope.v, vc_core::EVENT_SCHEMA_VERSION);
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_finishes_the_recording_in_flight() {
    // The capture thread may be part-way through writing a file. Exiting out from under it
    // would lose what the user just said.
    let harness = Harness::start(CONFIG).await;
    harness.send(Command::Start {
        profile: "dictate".to_owned(),
    });
    // Returning at all means the capture thread was joined rather than abandoned.
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recordings_directory_is_private() {
    use std::os::unix::fs::PermissionsExt;

    // These are recordings of the user speaking; the default umask is not a good enough
    // reason for anyone else on the machine to read them.
    let harness = Harness::start(CONFIG).await;
    assert!(
        until(|| harness.data.exists()).await,
        "data dir not created"
    );

    let mode = std::fs::metadata(&harness.data)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "recordings directory is mode {mode:o}");
    harness.stop().await;
}
