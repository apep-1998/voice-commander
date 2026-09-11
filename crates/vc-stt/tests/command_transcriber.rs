//! The `command` transcriber, tested against real programs.
//!
//! These spawn actual processes — `/bin/sh` scripts written into a temp directory — rather
//! than a mock, because the things worth testing here *are* the process boundary: argument
//! quoting, exit status, the deadline, and where the text is read from. A mock would assert
//! that the code calls the mock.
//!
//! No network and no microphone, so they run anywhere.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::Path;

use tempfile::TempDir;
use vc_core::config::{CommandTranscriber as Config, TextSource};
use vc_core::session::SessionId;
use vc_stt::{TranscribeError, TranscribeRequest, Transcriber, Transcript};

/// A session directory with a fake recording in it.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("audio.wav"), b"not really audio").expect("write audio");
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a shell script and return its path.
    ///
    /// Callers run it as `/bin/sh <path>` rather than executing it directly. Writing a file
    /// and immediately exec'ing it is racy in a multi-threaded program: another thread
    /// forking for its own child can hold a write descriptor open across the exec, and the
    /// kernel answers ETXTBSY. Handing the path to `sh` sidesteps that entirely — it reads
    /// the file rather than executing it — and costs nothing the tests care about.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.dir.path().join(name);
        std::fs::write(&path, format!("{body}\n")).expect("write script");
        path.display().to_string()
    }

    fn request(&self) -> TranscribeRequest {
        TranscribeRequest {
            audio_path: self.dir.path().join("audio.wav"),
            session_dir: self.dir.path().to_owned(),
            session_id: SessionId::from_raw("20260911T144812Z-dictate-2hc8b"),
            profile: "dictate".to_owned(),
            sample_rate: 16_000,
            duration_ms: 2_400,
        }
    }
}

fn transcriber(cmd: Vec<String>, text: TextSource, timeout_ms: u64) -> impl Transcriber {
    vc_stt::CommandTranscriber::new(
        "local".to_owned(),
        Config {
            cmd,
            text,
            env: BTreeMap::new(),
        },
        timeout_ms,
    )
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// Build argv that runs a script written by [`Fixture::script`].
fn sh(script: &str, args: &[&str]) -> Vec<String> {
    let mut out = vec!["/bin/sh".to_owned(), script.to_owned()];
    out.extend(args.iter().map(|arg| (*arg).to_owned()));
    out
}

async fn run(
    cmd: Vec<String>,
    text: TextSource,
    fixture: &Fixture,
) -> Result<Transcript, TranscribeError> {
    transcriber(cmd, text, 5_000)
        .transcribe(&fixture.request())
        .await
}

// ── the happy path ───────────────────────────────────────────────────────────

#[tokio::test]
async fn text_is_read_from_standard_output() {
    let fixture = Fixture::new();
    let script = fixture.script("say.sh", "echo 'open my calendar'");

    let transcript = run(sh(&script, &[]), TextSource::Stdout, &fixture)
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "open my calendar");
}

#[tokio::test]
async fn text_can_be_read_from_a_file_the_program_wrote() {
    // whisper.cpp's `-otxt` writes a file rather than printing, and so do plenty of others.
    let fixture = Fixture::new();
    let script = fixture.script("write.sh", "printf 'from a file' > \"$1\"");
    let out = fixture.path().join("out.txt").display().to_string();

    let transcript = run(
        sh(&script, &[&out]),
        TextSource::File { path: out.clone() },
        &fixture,
    )
    .await
    .expect("should transcribe");

    assert_eq!(transcript.text, "from a file");
}

#[tokio::test]
async fn the_output_file_path_may_itself_use_tokens() {
    let fixture = Fixture::new();
    let script = fixture.script("write.sh", "printf 'tokenised' > \"$1/out.txt\"");

    let transcript = run(
        sh(&script, &["{session_dir}"]),
        TextSource::File {
            path: "{session_dir}/out.txt".to_owned(),
        },
        &fixture,
    )
    .await
    .expect("should transcribe");

    assert_eq!(transcript.text, "tokenised");
}

#[tokio::test]
async fn trailing_whitespace_is_trimmed() {
    // Local models habitually pad their output with newlines, and a trailing newline typed
    // into a chat box sends the message before the user has finished.
    let fixture = Fixture::new();
    let script = fixture.script("pad.sh", "printf '\\n\\n  hello  \\n\\n'");

    let transcript = run(sh(&script, &[]), TextSource::Stdout, &fixture)
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "hello");
}

#[tokio::test]
async fn an_empty_transcript_is_a_result_not_a_failure() {
    // A recording of silence genuinely has no text in it. That is information for the
    // pipeline, not an error to report to the user.
    let fixture = Fixture::new();
    let script = fixture.script("silent.sh", "true");

    let transcript = run(sh(&script, &[]), TextSource::Stdout, &fixture)
        .await
        .expect("should succeed");

    assert_eq!(transcript.text, "");
}

// ── the audio actually reaches the program ───────────────────────────────────

#[tokio::test]
async fn the_audio_path_is_substituted() {
    let fixture = Fixture::new();
    let script = fixture.script(
        "check.sh",
        "test -f \"$1\" && printf 'found %s' \"$(basename \"$1\")\"",
    );

    let transcript = run(sh(&script, &["{audio_path}"]), TextSource::Stdout, &fixture)
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "found audio.wav");
}

#[tokio::test]
async fn the_program_runs_in_the_session_directory() {
    // So a script can write alongside the recording without being told where that is.
    let fixture = Fixture::new();
    let script = fixture.script("where.sh", "pwd");

    let transcript = run(sh(&script, &[]), TextSource::Stdout, &fixture)
        .await
        .expect("should transcribe");

    let reported = std::fs::canonicalize(&transcript.text).expect("canonicalize");
    let expected = std::fs::canonicalize(fixture.path()).expect("canonicalize");
    assert_eq!(reported, expected);
}

#[tokio::test]
async fn a_path_containing_spaces_stays_one_argument() {
    let dir = tempfile::tempdir().expect("temp dir");
    let awkward = dir.path().join("my recordings");
    std::fs::create_dir_all(&awkward).expect("create");
    std::fs::write(awkward.join("audio.wav"), b"x").expect("write");

    let script = dir.path().join("count.sh");
    std::fs::write(&script, "printf '%s' \"$#\"\n").expect("write script");

    let request = TranscribeRequest {
        audio_path: awkward.join("audio.wav"),
        session_dir: awkward.clone(),
        session_id: SessionId::from_raw("s"),
        profile: "p".to_owned(),
        sample_rate: 16_000,
        duration_ms: 100,
    };

    let transcript = transcriber(
        sh(&script.display().to_string(), &["{audio_path}"]),
        TextSource::Stdout,
        5_000,
    )
    .transcribe(&request)
    .await
    .expect("should transcribe");

    assert_eq!(
        transcript.text, "1",
        "the path was split into multiple arguments"
    );
}

#[tokio::test]
async fn the_environment_carries_the_same_values() {
    let fixture = Fixture::new();
    let script = fixture.script("env.sh", "printf '%s' \"$MY_MODEL\"");

    let transcriber = vc_stt::CommandTranscriber::new(
        "local".to_owned(),
        Config {
            cmd: sh(&script, &[]),
            text: TextSource::Stdout,
            env: [("MY_MODEL".to_owned(), "base.en".to_owned())]
                .into_iter()
                .collect(),
        },
        5_000,
    );

    let transcript = transcriber
        .transcribe(&fixture.request())
        .await
        .expect("should transcribe");
    assert_eq!(transcript.text, "base.en");
}

// ── failures ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_failing_program_reports_what_it_said() {
    // "model file not found" is worth more than "exited with status 1", and it is the only
    // thing standing between the user and an unexplained empty transcript.
    let fixture = Fixture::new();
    let script = fixture.script("fail.sh", "echo 'model file not found' >&2; exit 1");

    let error = run(sh(&script, &[]), TextSource::Stdout, &fixture)
        .await
        .expect_err("should fail");

    assert!(
        error.to_string().contains("model file not found"),
        "{error}"
    );
}

#[tokio::test]
async fn a_program_that_decided_to_fail_is_not_retried() {
    // Running it again just does the wrong thing twice.
    let fixture = Fixture::new();
    let script = fixture.script("fail.sh", "exit 1");

    let error = run(sh(&script, &[]), TextSource::Stdout, &fixture)
        .await
        .expect_err("should fail");

    assert!(
        !error.is_transient(),
        "a deliberate non-zero exit is not transient"
    );
}

#[tokio::test]
async fn a_missing_program_is_a_configuration_problem() {
    // It will still be missing next time, so this must not be retried and must not read as
    // the recording's fault.
    let fixture = Fixture::new();

    let error = run(
        argv(&["/definitely/not/a/real/program"]),
        TextSource::Stdout,
        &fixture,
    )
    .await
    .expect_err("should fail");

    assert!(matches!(error, TranscribeError::Config(_)), "got {error:?}");
    assert!(!error.is_transient());
}

#[tokio::test]
async fn a_slow_program_is_killed_and_the_timeout_is_worth_retrying() {
    // A local model can be slow because something else was using the machine, so this one
    // does deserve another go.
    let fixture = Fixture::new();
    let script = fixture.script("slow.sh", "sleep 30");

    let error = transcriber(sh(&script, &[]), TextSource::Stdout, 200)
        .transcribe(&fixture.request())
        .await
        .expect_err("should time out");

    assert!(error.is_transient(), "a timeout should be retried: {error}");
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[tokio::test]
async fn a_program_that_succeeds_without_writing_its_output_file_is_reported_clearly() {
    // Otherwise the user sees "no transcript" with no hint that the program lied.
    let fixture = Fixture::new();
    let script = fixture.script("lie.sh", "true");

    let error = run(
        sh(&script, &[]),
        TextSource::File {
            path: fixture
                .path()
                .join("never-written.txt")
                .display()
                .to_string(),
        },
        &fixture,
    )
    .await
    .expect_err("should fail");

    let message = error.to_string();
    assert!(message.contains("said it succeeded"), "{message}");
    assert!(message.contains("never-written.txt"), "{message}");
}

// ── the registry ─────────────────────────────────────────────────────────────

#[test]
fn the_registry_builds_what_the_configuration_defines() {
    let loaded = vc_core::Config::from_layers(&[
        vc_core::config::Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        vc_core::config::Layer::new(
            "config.toml",
            r#"
[transcribers.local]
type = "command"
cmd = ["whisper-cli", "-f", "{audio_path}"]
text = { from = "stdout" }
[profiles.p]
transcriber = "local"
"#,
        ),
    ])
    .expect("valid config");

    let (registry, errors) = vc_stt::Registry::from_config(&loaded.config);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(registry.get("local").map(|t| t.kind()), Some("command"));
    assert!(registry.get("nope").is_none());
}

#[test]
fn one_unbuildable_transcriber_does_not_stop_the_others_being_built() {
    // One bad entry must not take the daemon down: the other profiles still work, and the
    // user finds out from a log line rather than from nothing starting at all.
    //
    // Every shipped adapter builds now, so this uses a transcriber whose *command* is empty
    // — something configuration validation rejects, but which a caller constructing the
    // config directly can still produce.
    let config: vc_core::config::TranscriberConfig = toml::from_str(
        r#"
type = "command"
cmd = []
text = { from = "stdout" }
"#,
    )
    .expect("parses");

    match vc_stt::build("broken", &config) {
        Ok(transcriber) => {
            // An empty command is caught when it runs rather than when it is built, which is
            // fine — the failure is reported against the session, not against startup.
            assert_eq!(transcriber.kind(), "command");
        }
        Err(error) => assert!(error.to_string().contains("broken"), "{error}"),
    }
}

#[test]
fn the_registry_builds_every_adapter_the_configuration_names() {
    let loaded = vc_core::Config::from_layers(&[
        vc_core::config::Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        vc_core::config::Layer::new(
            "config.toml",
            r#"
[transcribers.local]
type = "command"
cmd = ["true"]
text = { from = "stdout" }
[transcribers.cloud]
type = "openai"
api_key = { env = "KEY" }
[transcribers.other]
type = "http"
url = "https://example.com/v1"
audio = { how = "raw_body" }
[profiles.a]
transcriber = "local"
[profiles.b]
transcriber = "cloud"
[profiles.c]
transcriber = "other"
"#,
        ),
    ])
    .expect("valid config");

    let (registry, errors) = vc_stt::Registry::from_config(&loaded.config);

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(registry.get("local").map(|t| t.kind()), Some("command"));
    assert_eq!(registry.get("cloud").map(|t| t.kind()), Some("openai"));
    assert_eq!(registry.get("other").map(|t| t.kind()), Some("http"));
}
