//! Integration tests against a real daemon on a real socket.
//!
//! These start the actual `Daemon` and talk to it with the actual `Client` — the same code
//! path a Hyprland keybind takes. A test against a mock would prove nothing about the accept
//! loop, the framing, or the socket's permissions, which is most of what can go wrong here.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the harness below is plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::oneshot;
use vc_ipc::protocol::{ActivityState, Command, ErrorCode, Request, Response};
use vc_ipc::{Client, ClientError};

const TIMEOUT: Duration = Duration::from_secs(5);

// Every test here runs on a multi-threaded runtime. The client is deliberately synchronous —
// it has to be, since it runs on every keypress — so calling it from a test body blocks that
// thread. On a single-threaded runtime that would block the daemon task itself, and nothing
// would ever answer. This is also how the daemon really runs.

/// A daemon running on a private socket, shut down when this is dropped.
struct Harness {
    socket: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    joined: Option<tokio::task::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn start(config: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("config.toml"), config).expect("write config");

        // Inside the temp dir rather than the real runtime directory, so a test run never
        // collides with the daemon the developer actually uses.
        let socket = dir.path().join("vc.sock");
        let daemon = vc_daemon::Daemon::with_storage(
            dir.path(),
            socket.clone(),
            Some(dir.path().join("data")),
        )
        .expect("daemon");

        let (tx, rx) = oneshot::channel();
        let joined = tokio::spawn(async move {
            daemon
                .run(async {
                    let _ = rx.await;
                })
                .await
                .expect("daemon should exit cleanly");
        });

        wait_until_listening(&socket).await;

        Self {
            socket,
            shutdown: Some(tx),
            joined: Some(joined),
            _dir: dir,
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket, TIMEOUT).expect("the daemon should be reachable")
    }

    fn send(&self, command: Command) -> Response {
        self.client().send(command).expect("command should succeed")
    }

    fn send_expecting_error(&self, command: Command) -> (ErrorCode, String) {
        match self.client().send(command) {
            Err(ClientError::Refused { code, message }) => (code, message),
            other => panic!("expected a refusal, got {other:?}"),
        }
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

/// Poll until the socket answers, rather than sleeping a fixed amount and hoping.
async fn wait_until_listening(socket: &Path) {
    for _ in 0..200 {
        if vc_ipc::client::is_running(socket, Duration::from_millis(100)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the daemon never started listening on {}", socket.display());
}

const CONFIG: &str = r#"
[sinks.noop]
type = "command"
cmd = ["true"]
[profiles.dictate]
sinks = ["noop"]
[profiles.memo]
sinks = ["noop"]
"#;

// ── the basics ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_ping_is_answered_with_the_version_and_protocol() {
    let harness = Harness::start(CONFIG).await;
    match harness.send(Command::Ping) {
        Response::Pong { protocol, .. } => assert_eq!(protocol, vc_ipc::PROTOCOL_VERSION),
        other => panic!("expected a pong, got {other:?}"),
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_idle_and_lists_the_configured_profiles() {
    let harness = Harness::start(CONFIG).await;
    match harness.send(Command::Status) {
        Response::Status(status) => {
            assert_eq!(status.activity, ActivityState::Idle);
            // Sorted, because `status` doubles as "what can I bind?".
            assert_eq!(status.profiles, vec!["default", "dictate", "memo"]);
            assert_eq!(status.device, None);
        }
        other => panic!("expected a status, got {other:?}"),
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_is_not_readable_by_anyone_else() {
    // It carries commands that record audio from the user's microphone.
    use std::os::unix::fs::PermissionsExt;

    let harness = Harness::start(CONFIG).await;
    let mode = std::fs::metadata(&harness.socket)
        .expect("socket exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "socket mode was {mode:o}, expected 600");
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_is_removed_on_shutdown() {
    let harness = Harness::start(CONFIG).await;
    let socket = harness.socket.clone();
    harness.stop().await;
    assert!(
        !socket.exists(),
        "a socket left behind makes the next start have to reason about it"
    );
}

// ── error reporting ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_profile_is_named_along_with_the_ones_that_exist() {
    // This is the most likely mistake in a keybind, and the message is the only feedback a
    // user gets — nothing is printed on success.
    let harness = Harness::start(CONFIG).await;
    let (code, message) = harness.send_expecting_error(Command::Start {
        profile: "dictat".to_owned(),
    });

    assert_eq!(code, ErrorCode::UnknownProfile);
    assert!(message.contains("dictat"), "{message}");
    assert!(
        message.contains("dictate"),
        "should list what does exist: {message}"
    );
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_known_profile_is_accepted_immediately() {
    // The reply must not wait for a device to open: this runs on a keypress, and the point
    // of the daemon is that pressing a key is instant.
    let harness = Harness::start(CONFIG).await;
    match harness.send(Command::Start {
        profile: "dictate".to_owned(),
    }) {
        Response::Accepted { .. } => {}
        other => panic!("expected acceptance, got {other:?}"),
    }
    harness.send(Command::Stop {
        profile: "dictate".to_owned(),
    });
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_failure_gets_its_own_exit_code() {
    // A wrapper script should be able to branch without parsing English.
    assert_ne!(
        ErrorCode::UnknownProfile.exit_code(),
        ErrorCode::DeviceUnavailable.exit_code()
    );
    assert_ne!(ErrorCode::BadRequest.exit_code(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_speaking_a_different_protocol_is_told_so() {
    // Upgrading the package while the old daemon is still running is entirely normal.
    let harness = Harness::start(CONFIG).await;

    let raw = format!("{}\n", serde_json::json!({ "v": 999, "cmd": "ping" }));
    let reply = raw_exchange(&harness.socket, &raw);
    let response: Response = serde_json::from_str(&reply).expect("a reply");

    match response {
        Response::Error { code, message } => {
            assert_eq!(code, ErrorCode::VersionMismatch);
            assert!(
                message.contains("restart"),
                "should say what to do: {message}"
            );
        }
        other => panic!("expected a version mismatch, got {other:?}"),
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_input_is_refused_without_killing_the_connection() {
    let harness = Harness::start(CONFIG).await;
    let mut client = harness.client();

    match client.send(Command::Ping) {
        Ok(Response::Pong { .. }) => {}
        other => panic!("expected a pong, got {other:?}"),
    }
    // And the daemon survives the garbage well enough to keep serving everyone else.
    let reply = raw_exchange(&harness.socket, "not json at all\n");
    assert!(reply.contains("bad_request"), "{reply}");
    match harness.send(Command::Ping) {
        Response::Pong { .. } => {}
        other => panic!("the daemon stopped answering after bad input: {other:?}"),
    }
    harness.stop().await;
}

// ── the protocol is usable without our client ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_protocol_can_be_driven_with_plain_lines_of_json() {
    // Line-delimited JSON was chosen so the daemon can be driven and inspected from a shell
    // with socat. If that stops working, the choice has stopped paying for itself.
    let harness = Harness::start(CONFIG).await;
    let reply = raw_exchange(&harness.socket, "{\"v\":1,\"cmd\":\"status\"}\n");
    assert!(reply.contains("\"reply\":\"status\""), "{reply}");
    assert!(reply.contains("dictate"), "{reply}");
    harness.stop().await;
}

// ── concurrency ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn several_clients_are_served_at_once() {
    let harness = Harness::start(CONFIG).await;
    let socket = harness.socket.clone();

    let mut handles = Vec::new();
    for _ in 0..16 {
        let socket = socket.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let mut client = Client::connect(&socket, TIMEOUT).expect("connect");
            matches!(client.send(Command::Ping), Ok(Response::Pong { .. }))
        }));
    }
    for handle in handles {
        assert!(handle.await.expect("task"), "a concurrent client failed");
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_does_not_block_anyone_else() {
    // A `subscribe` holds its connection open indefinitely. If the accept loop served
    // clients one at a time, the first indicator to connect would wedge every keybind.
    let harness = Harness::start(CONFIG).await;
    let socket = harness.socket.clone();

    let subscriber = tokio::task::spawn_blocking(move || {
        let client = Client::connect(&socket, TIMEOUT).expect("connect");
        let mut stream = client.subscribe(vec![]).expect("subscribe");
        // Block on the stream, as a real indicator would between recordings.
        stream.next()
    });

    // Meanwhile, ordinary commands must still work.
    for _ in 0..3 {
        match harness.send(Command::Ping) {
            Response::Pong { .. } => {}
            other => panic!("a subscriber blocked an ordinary command: {other:?}"),
        }
    }

    harness.stop().await;
    let _ = subscriber.await;
}

// ── reload ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_picks_up_a_profile_added_while_running() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("config.toml"), CONFIG).expect("write config");
    let socket = dir.path().join("vc.sock");
    let daemon =
        vc_daemon::Daemon::with_storage(dir.path(), socket.clone(), Some(dir.path().join("data")))
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
    wait_until_listening(&socket).await;

    let mut client = Client::connect(&socket, TIMEOUT).expect("connect");
    let (code, _) = match client.send(Command::Start {
        profile: "later".to_owned(),
    }) {
        Err(ClientError::Refused { code, message }) => (code, message),
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert_eq!(code, ErrorCode::UnknownProfile);

    std::fs::write(
        dir.path().join("config.toml"),
        format!("{CONFIG}\n[profiles.later]\nsinks = [\"noop\"]\n"),
    )
    .expect("rewrite config");

    let mut client = Client::connect(&socket, TIMEOUT).expect("connect");
    match client.send(Command::Reload) {
        Ok(Response::Reloaded { .. }) => {}
        other => panic!("expected a reload, got {other:?}"),
    }

    let mut client = Client::connect(&socket, TIMEOUT).expect("connect");
    match client.send(Command::Start {
        profile: "later".to_owned(),
    }) {
        Ok(Response::Accepted { .. }) => {}
        other => panic!("expected the new profile to resolve, got {other:?}"),
    }

    let _ = tx.send(());
    let _ = joined.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_config_is_refused_and_the_running_one_survives() {
    // The daemon holds the microphone. A typo saved in an editor must not end the user's
    // ability to record until they notice and restart something.
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("config.toml"), CONFIG).expect("write config");
    let socket = dir.path().join("vc.sock");
    let daemon =
        vc_daemon::Daemon::with_storage(dir.path(), socket.clone(), Some(dir.path().join("data")))
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
    wait_until_listening(&socket).await;

    std::fs::write(dir.path().join("config.toml"), "[profiles.broken\n").expect("rewrite");

    let mut client = Client::connect(&socket, TIMEOUT).expect("connect");
    match client.send(Command::Reload) {
        Err(ClientError::Refused { code, message }) => {
            assert_eq!(code, ErrorCode::ConfigInvalid);
            assert!(
                message.contains("still in effect"),
                "the user needs to know nothing broke: {message}"
            );
        }
        other => panic!("expected the reload to be refused, got {other:?}"),
    }

    // The profiles from before the bad edit are still there.
    let mut client = Client::connect(&socket, TIMEOUT).expect("connect");
    match client.send(Command::Status) {
        Ok(Response::Status(status)) => assert!(status.profiles.contains(&"dictate".to_owned())),
        other => panic!("expected a status, got {other:?}"),
    }

    let _ = tx.send(());
    let _ = joined.await;
}

// ── stale sockets ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_socket_left_behind_by_a_crash_is_cleared() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("config.toml"), CONFIG).expect("write config");
    let socket = dir.path().join("vc.sock");

    // A plain file where the socket goes, as a crashed daemon would leave.
    std::fs::write(&socket, b"stale").expect("write stale socket");

    let daemon =
        vc_daemon::Daemon::with_storage(dir.path(), socket.clone(), Some(dir.path().join("data")))
            .expect("daemon");
    let (tx, rx) = oneshot::channel();
    let joined = tokio::spawn(async move {
        daemon
            .run(async {
                let _ = rx.await;
            })
            .await
            .expect("should start anyway");
    });
    wait_until_listening(&socket).await;

    let _ = tx.send(());
    let _ = joined.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_refuses_to_steal_a_live_socket() {
    // Deleting a socket on sight would let a second daemon take it from a healthy first one,
    // and the two would then fight over the microphone.
    let harness = Harness::start(CONFIG).await;
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("config.toml"), CONFIG).expect("write config");

    let intruder = vc_daemon::Daemon::with_storage(
        dir.path(),
        harness.socket.clone(),
        Some(dir.path().join("data")),
    )
    .expect("daemon");
    let error = intruder
        .run(std::future::pending())
        .await
        .expect_err("should refuse to bind");
    assert!(error.to_string().contains("already listening"), "{error}");

    // And the first one is untouched.
    match harness.send(Command::Ping) {
        Response::Pong { .. } => {}
        other => panic!("the original daemon was disturbed: {other:?}"),
    }
    harness.stop().await;
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Write raw bytes to the socket and read one line back, bypassing our own client.
fn raw_exchange(socket: &Path, request: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(TIMEOUT))
        .expect("set read timeout");
    stream.write_all(request.as_bytes()).expect("write");
    stream.flush().expect("flush");

    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).expect("read");
    line
}

/// Keeps `Request` used here, so the protocol types stay exercised from the test side too.
#[test]
fn a_request_serializes_to_the_shape_the_daemon_parses() {
    let line = serde_json::to_string(&Request::new(Command::Start {
        profile: "dictate".to_owned(),
    }))
    .expect("serializes");
    assert_eq!(line, r#"{"v":1,"cmd":"start","profile":"dictate"}"#);
}
