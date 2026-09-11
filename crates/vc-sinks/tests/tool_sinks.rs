//! The sinks that drive an external tool, and the ones that touch the filesystem or network.
//!
//! The tool-driven sinks are tested through a fake runner that records the exact command
//! line, so these run on a machine with none of `wl-copy`, `wtype` or `notify-send`
//! installed — which is the difference between testing them and hoping.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use vc_core::config::{AudioFormat, CaptureMode, GapMode, SinkConfig, TriggerMode};
use vc_core::session::{
    AudioSummary, CaptureSummary, LevelSummary, Outcome, SessionId, SessionRecord,
};
use vc_exec::{CommandOutput, CommandSpec, ExecError, Runner};
use vc_sinks::{SinkContext, SinkError};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A runner that records what it was asked to run instead of running it.
#[derive(Debug)]
struct Fake {
    calls: Mutex<Vec<Vec<String>>>,
    /// Programs this machine pretends to have.
    installed: Vec<String>,
    status: Option<i32>,
    stderr: String,
    missing: bool,
}

impl Fake {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            installed: vec!["wtype".to_owned(), "ydotool".to_owned()],
            status: Some(0),
            stderr: String::new(),
            missing: false,
        }
    }

    fn with_installed(installed: &[&str]) -> Self {
        Self {
            installed: installed.iter().map(|p| (*p).to_owned()).collect(),
            ..Self::new()
        }
    }

    fn failing(stderr: &str) -> Self {
        Self {
            status: Some(1),
            stderr: stderr.to_owned(),
            ..Self::new()
        }
    }

    fn not_installed() -> Self {
        Self {
            missing: true,
            installed: Vec::new(),
            ..Self::new()
        }
    }

    fn argv(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("lock")
            .first()
            .cloned()
            .unwrap_or_default()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("lock").len()
    }
}

#[async_trait::async_trait]
impl Runner for Fake {
    async fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, ExecError> {
        self.calls.lock().expect("lock").push(spec.argv.clone());
        if self.missing {
            return Err(ExecError::NotFound {
                program: spec.program().to_owned(),
            });
        }
        Ok(CommandOutput {
            status: self.status,
            stdout: String::new(),
            stderr: self.stderr.clone(),
            timed_out: false,
            elapsed: std::time::Duration::ZERO,
        })
    }

    fn has(&self, program: &str) -> bool {
        self.installed.iter().any(|p| p == program)
    }
}

fn context(text: Option<&str>, audio: &str) -> SinkContext {
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
            path: audio.into(),
            format: AudioFormat::Wav,
            sample_rate: 16_000,
            channels: 1,
            bytes: 100,
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
    SinkContext::new(record, text.map(str::to_owned), None)
}

fn sink(toml_text: &str, runner: Arc<Fake>) -> Arc<dyn vc_sinks::Sink> {
    let config: SinkConfig = toml::from_str(toml_text).expect("parses");
    vc_sinks::registry::build_with("test", &config, runner).expect("builds")
}

// ── clipboard ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_clipboard_sink_calls_wl_copy_with_the_text() {
    let runner = Arc::new(Fake::new());
    sink("type = \"clipboard\"", Arc::clone(&runner))
        .deliver(&context(Some("open my calendar"), "/a.wav"))
        .await
        .expect("should copy");

    assert_eq!(
        runner.argv(),
        vec!["wl-copy", "--", "open my calendar"],
        "the exact command line matters"
    );
}

#[tokio::test]
async fn a_transcript_starting_with_a_dash_is_text_not_an_option() {
    // "-- delete that" would otherwise be parsed by wl-copy as flags.
    let runner = Arc::new(Fake::new());
    sink("type = \"clipboard\"", Arc::clone(&runner))
        .deliver(&context(Some("--version please"), "/a.wav"))
        .await
        .expect("should copy");

    let argv = runner.argv();
    assert_eq!(argv[argv.len() - 2], "--");
    assert_eq!(argv[argv.len() - 1], "--version please");
}

#[tokio::test]
async fn the_primary_selection_is_opt_in() {
    let runner = Arc::new(Fake::new());
    sink("type = \"clipboard\"\nprimary = true", Arc::clone(&runner))
        .deliver(&context(Some("hi"), "/a.wav"))
        .await
        .expect("should copy");

    assert!(runner.argv().contains(&"--primary".to_owned()));
}

#[tokio::test]
async fn a_missing_tool_says_what_it_was_for() {
    // "No such file or directory (os error 2)" tells a user nothing.
    let runner = Arc::new(Fake::not_installed());
    let error = sink("type = \"clipboard\"", runner)
        .deliver(&context(Some("hi"), "/a.wav"))
        .await
        .expect_err("should fail");

    assert!(matches!(error, SinkError::Config(_)), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("wl-copy is not installed"), "{message}");
    assert!(message.contains("clipboard"), "{message}");
}

// ── typing ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn typing_prefers_wtype_when_both_are_available() {
    // wtype speaks the Wayland virtual-keyboard protocol directly; ydotool needs a daemon
    // and access to /dev/uinput.
    let runner = Arc::new(Fake::new());
    sink("type = \"type\"", Arc::clone(&runner))
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect("should type");

    assert_eq!(runner.argv(), vec!["wtype", "--", "hello"]);
}

#[tokio::test]
async fn typing_falls_back_to_ydotool_when_wtype_is_absent() {
    let runner = Arc::new(Fake::with_installed(&["ydotool"]));
    sink("type = \"type\"", Arc::clone(&runner))
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect("should type");

    assert_eq!(runner.argv(), vec!["ydotool", "type", "--", "hello"]);
}

#[tokio::test]
async fn an_explicit_tool_is_used_even_when_the_other_is_present() {
    let runner = Arc::new(Fake::new());
    sink("type = \"type\"\ntool = \"ydotool\"", Arc::clone(&runner))
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect("should type");

    assert_eq!(runner.argv()[0], "ydotool");
}

#[tokio::test]
async fn typing_with_no_tool_installed_explains_what_to_install() {
    let runner = Arc::new(Fake::with_installed(&[]));
    let error = sink("type = \"type\"", runner)
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect_err("should fail");

    let message = error.to_string();
    assert!(message.contains("wtype"), "{message}");
    assert!(message.contains("ydotool"), "{message}");
}

#[tokio::test]
async fn a_key_delay_is_passed_through_in_each_tools_own_spelling() {
    let runner = Arc::new(Fake::new());
    sink(
        "type = \"type\"\ntool = \"wtype\"\nkey_delay_ms = 12",
        Arc::clone(&runner),
    )
    .deliver(&context(Some("hi"), "/a.wav"))
    .await
    .expect("should type");
    assert_eq!(runner.argv(), vec!["wtype", "-d", "12", "--", "hi"]);

    let runner = Arc::new(Fake::new());
    sink(
        "type = \"type\"\ntool = \"ydotool\"\nkey_delay_ms = 12",
        Arc::clone(&runner),
    )
    .deliver(&context(Some("hi"), "/a.wav"))
    .await
    .expect("should type");
    assert_eq!(
        runner.argv(),
        vec!["ydotool", "type", "--key-delay", "12", "--", "hi"]
    );
}

#[tokio::test]
async fn typing_nothing_does_not_invoke_the_tool_at_all() {
    // Invoking it with an empty string can still disturb focus, for no benefit.
    let runner = Arc::new(Fake::new());
    sink("type = \"type\"", Arc::clone(&runner))
        .deliver(&context(Some(""), "/a.wav"))
        .await
        .expect("should be a no-op");

    assert_eq!(runner.call_count(), 0);
}

// ── notifications ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_notification_expands_tokens_in_its_summary_and_body() {
    let runner = Arc::new(Fake::new());
    sink(
        "type = \"notify\"\nsummary = \"{profile}\"\nbody = \"{text}\"",
        Arc::clone(&runner),
    )
    .deliver(&context(Some("open my calendar"), "/a.wav"))
    .await
    .expect("should notify");

    let argv = runner.argv();
    assert_eq!(argv[0], "notify-send");
    assert_eq!(argv[argv.len() - 2], "dictate");
    assert_eq!(argv[argv.len() - 1], "open my calendar");
}

#[tokio::test]
async fn a_notification_is_useful_without_a_transcript() {
    // "your recording finished" is worth saying even when nothing transcribed it.
    let config: SinkConfig = toml::from_str("type = \"notify\"").expect("parses");
    assert!(!config.needs_text());
}

#[tokio::test]
async fn urgency_and_timeout_reach_the_command_line() {
    let runner = Arc::new(Fake::new());
    sink(
        "type = \"notify\"\nurgency = \"critical\"\nexpire_ms = 9000",
        Arc::clone(&runner),
    )
    .deliver(&context(Some("hi"), "/a.wav"))
    .await
    .expect("should notify");

    let argv = runner.argv();
    assert!(argv.windows(2).any(|w| w == ["-u", "critical"]), "{argv:?}");
    assert!(argv.windows(2).any(|w| w == ["-t", "9000"]), "{argv:?}");
}

#[tokio::test]
async fn a_tool_that_fails_reports_what_it_said() {
    let runner = Arc::new(Fake::failing("cannot connect to the notification daemon"));
    let error = sink("type = \"notify\"", runner)
        .deliver(&context(Some("hi"), "/a.wav"))
        .await
        .expect_err("should fail");

    assert!(error.to_string().contains("cannot connect"), "{error}");
}

// ── the file sink ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_file_sink_appends_a_templated_line() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("journal.md");
    let config: SinkConfig = toml::from_str(&format!(
        "type = \"file\"\npath = {:?}\ntemplate = \"- {{text}}\\n\"",
        path.display().to_string()
    ))
    .expect("parses");
    let sink = vc_sinks::registry::build_with("j", &config, Arc::new(Fake::new())).expect("builds");

    sink.deliver(&context(Some("first"), "/a.wav"))
        .await
        .expect("write");
    sink.deliver(&context(Some("second"), "/a.wav"))
        .await
        .expect("write");

    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "- first\n- second\n"
    );
}

#[tokio::test]
async fn the_file_sink_creates_the_directory_it_needs() {
    // `~/notes/voice/{date}.md` on the first day of use points somewhere that does not exist
    // yet, and telling the user to go and mkdir it is a poor first impression.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("notes").join("voice").join("2026-09-11.md");
    let config: SinkConfig = toml::from_str(&format!(
        "type = \"file\"\npath = {:?}\ntemplate = \"{{text}}\"",
        path.display().to_string()
    ))
    .expect("parses");

    vc_sinks::registry::build_with("j", &config, Arc::new(Fake::new()))
        .expect("builds")
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect("write");

    assert_eq!(std::fs::read_to_string(&path).expect("read"), "hello");
}

#[tokio::test]
async fn a_date_token_in_the_path_gives_a_file_per_day() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config: SinkConfig = toml::from_str(&format!(
        "type = \"file\"\npath = \"{}/{{date}}.md\"\ntemplate = \"{{text}}\\n\"",
        dir.path().display()
    ))
    .expect("parses");

    vc_sinks::registry::build_with("j", &config, Arc::new(Fake::new()))
        .expect("builds")
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect("write");

    assert!(dir.path().join("2026-09-11.md").exists());
}

// ── the http sink ────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_http_sink_posts_the_session_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("content-type", "application/json"))
        .and(body_string_contains("20260911T144812Z-dictate-2hc8b"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let config: SinkConfig =
        toml::from_str(&format!("type = \"http\"\nurl = \"{}/hook\"", server.uri()))
            .expect("parses");

    vc_sinks::registry::build_with("w", &config, Arc::new(Fake::new()))
        .expect("builds")
        .deliver(&context(Some("hello"), "/a.wav"))
        .await
        .expect("should post");
}

#[tokio::test]
async fn an_http_sink_can_send_a_body_template() {
    let server = MockServer::start().await;
    Mock::given(body_string_contains(r#""said": "open my calendar""#))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let config: SinkConfig = toml::from_str(&format!(
        r#"
type = "http"
url = "{}/hook"
body = {{ kind = "template", template = "{{\"said\": \"{{text}}\"}}" }}
"#,
        server.uri()
    ))
    .expect("parses");

    vc_sinks::registry::build_with("w", &config, Arc::new(Fake::new()))
        .expect("builds")
        .deliver(&context(Some("open my calendar"), "/a.wav"))
        .await
        .expect("should post");
}

#[tokio::test]
async fn a_webhook_that_is_busy_is_worth_retrying_and_a_wrong_url_is_not() {
    let server = MockServer::start().await;
    Mock::given(path("/busy"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(path("/gone"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    for (route, transient) in [("busy", true), ("gone", false)] {
        let config: SinkConfig = toml::from_str(&format!(
            "type = \"http\"\nurl = \"{}/{route}\"",
            server.uri()
        ))
        .expect("parses");

        let error = vc_sinks::registry::build_with("w", &config, Arc::new(Fake::new()))
            .expect("builds")
            .deliver(&context(Some("hi"), "/a.wav"))
            .await
            .expect_err("should fail");

        assert_eq!(
            error.is_transient(),
            transient,
            "/{route} was classified wrongly: {error}"
        );
    }
}

// ── every kind now builds ────────────────────────────────────────────────────

#[test]
fn the_shipped_example_config_builds_every_callback_it_defines() {
    let loaded = vc_core::Config::from_layers(&[
        vc_core::config::Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        vc_core::config::Layer::new("config.toml", vc_core::config::EXAMPLE_CONFIG),
    ])
    .expect("the shipped example is valid");

    let (registry, errors) = vc_sinks::Registry::from_config(&loaded.config);
    assert!(errors.is_empty(), "{errors:?}");

    for name in [
        "agent",
        "archive",
        "webhook",
        "clipboard",
        "type_it",
        "journal",
    ] {
        assert!(registry.get(name).is_some(), "{name} did not build");
    }
}
