//! The HTTP transcribers, against a real local server.
//!
//! `wiremock` runs an actual HTTP server on localhost and the adapter makes actual requests
//! to it, so the multipart encoding, the headers and the status handling are all exercised
//! for real. No outbound network: nothing here talks to a provider, and nothing needs a key.

// clippy.toml permits panicking in tests, but that only covers `#[test]` functions —
// the helpers below are plain functions, so the allowance is stated here too.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use vc_core::config::{
    AudioAttach, HttpMethod, HttpTranscriber as HttpConfig, OpenAiTranscriber as OpenAiConfig,
    ResponseExtract, ResponseFormat, SecretRef,
};
use vc_core::session::SessionId;
use vc_stt::{TranscribeError, TranscribeRequest, Transcriber};
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A session directory holding something that stands in for a recording.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("audio.wav"), b"RIFF....WAVEfake").expect("write audio");
        Self { dir }
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

fn http_config(url: String) -> HttpConfig {
    HttpConfig {
        url,
        method: HttpMethod::Post,
        headers: BTreeMap::new(),
        audio: AudioAttach::Multipart {
            field: "file".to_owned(),
        },
        form: BTreeMap::new(),
        json: BTreeMap::new(),
        query: BTreeMap::new(),
        response: ResponseExtract::default(),
    }
}

fn openai_config(base_url: String) -> OpenAiConfig {
    OpenAiConfig {
        model: "gpt-4o-transcribe".to_owned(),
        base_url,
        api_key: SecretRef {
            env: Some("VC_TEST_OPENAI_KEY".to_owned()),
            command: None,
        },
        language: None,
        prompt: None,
        temperature: None,
    }
}

// ── the generic adapter ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_multipart_request_carries_the_audio_and_the_form_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(body_string_contains("whisper-large-v3-turbo"))
        // The bytes of the file must actually be in the body.
        .and(body_string_contains("WAVEfake"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "text": "open my calendar"
        })))
        .mount(&server)
        .await;

    let mut config = http_config(format!("{}/v1/audio/transcriptions", server.uri()));
    config
        .form
        .insert("model".to_owned(), "whisper-large-v3-turbo".to_owned());

    let fixture = Fixture::new();
    let transcript = vc_stt::HttpTranscriber::new("groq".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "open my calendar");
}

#[tokio::test]
async fn the_audio_part_declares_a_content_type() {
    // Providers reject a part with no content type, or silently treat it as text. This is
    // the most common reason a hand-written `http` transcriber gets a 400.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("audio/wav"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ok"})))
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    vc_stt::HttpTranscriber::new("p".to_owned(), http_config(server.uri()), 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("the part should declare audio/wav");
}

#[tokio::test]
async fn environment_references_in_headers_are_expanded_at_request_time() {
    // Not at load time: rotating a key should not require reloading the daemon.
    std::env::set_var("VC_TEST_GROQ_KEY", "sk-from-the-environment");

    let server = MockServer::start().await;
    Mock::given(header("authorization", "Bearer sk-from-the-environment"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ok"})))
        .mount(&server)
        .await;

    let mut config = http_config(server.uri());
    config.headers.insert(
        "Authorization".to_owned(),
        "Bearer ${VC_TEST_GROQ_KEY}".to_owned(),
    );

    let fixture = Fixture::new();
    vc_stt::HttpTranscriber::new("groq".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("the header should carry the expanded key");
}

#[tokio::test]
async fn raw_body_sends_the_file_as_the_whole_request() {
    // What Deepgram expects.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("model", "nova-2"))
        .and(body_string_contains("WAVEfake"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": {"channels": [{"alternatives": [{"transcript": "from deepgram"}]}]}
        })))
        .mount(&server)
        .await;

    let mut config = http_config(server.uri());
    config.audio = AudioAttach::RawBody;
    config.query.insert("model".to_owned(), "nova-2".to_owned());
    config.response = ResponseExtract {
        format: ResponseFormat::Json,
        text_pointer: "/results/channels/0/alternatives/0/transcript".to_owned(),
    };

    let fixture = Fixture::new();
    let transcript = vc_stt::HttpTranscriber::new("deepgram".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "from deepgram");
}

#[tokio::test]
async fn base64_json_sends_the_audio_inside_a_json_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        // "RIFF....WAVEfake" base64-encoded.
        .and(body_string_contains("UklGRi4uLi5XQVZFZmFrZQ=="))
        .and(body_string_contains("some-model"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "b64"})))
        .mount(&server)
        .await;

    let mut config = http_config(server.uri());
    config.audio = AudioAttach::Base64Json {
        field: "audio".to_owned(),
    };
    config
        .json
        .insert("model".to_owned(), "some-model".to_owned());

    let fixture = Fixture::new();
    let transcript = vc_stt::HttpTranscriber::new("p".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "b64");
}

#[tokio::test]
async fn a_plain_text_response_is_taken_whole() {
    // A self-hosted whisper.cpp server can be configured to return bare text.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("  bare text \n"))
        .mount(&server)
        .await;

    let mut config = http_config(server.uri());
    config.response = ResponseExtract {
        format: ResponseFormat::Text,
        text_pointer: String::new(),
    };

    let fixture = Fixture::new();
    let transcript = vc_stt::HttpTranscriber::new("local".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("should transcribe");

    assert_eq!(transcript.text, "bare text");
}

// ── failures ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bad_key_is_reported_as_configuration_and_not_retried() {
    // A wrong key will still be wrong in 500ms, and retrying can trip rate limiting on top.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"message": "Incorrect API key provided"}
        })))
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    let error = vc_stt::HttpTranscriber::new("p".to_owned(), http_config(server.uri()), 5_000)
        .transcribe(&fixture.request())
        .await
        .expect_err("should fail");

    assert!(matches!(error, TranscribeError::Config(_)), "{error:?}");
    assert!(!error.is_transient());
    assert!(
        error.to_string().contains("Incorrect API key provided"),
        "the provider's own message is the useful part: {error}"
    );
}

#[tokio::test]
async fn a_busy_provider_is_worth_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    let error = vc_stt::HttpTranscriber::new("p".to_owned(), http_config(server.uri()), 5_000)
        .transcribe(&fixture.request())
        .await
        .expect_err("should fail");

    assert!(error.is_transient(), "a 503 should be retried: {error}");
}

#[tokio::test]
async fn a_wrong_pointer_says_what_the_response_looked_like() {
    // Otherwise the user is left with "no transcript" and a working API call.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": {"transcript": "it was here all along"}
        })))
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    let error = vc_stt::HttpTranscriber::new("p".to_owned(), http_config(server.uri()), 5_000)
        .transcribe(&fixture.request())
        .await
        .expect_err("should fail");

    let message = error.to_string();
    assert!(message.contains("/text"), "{message}");
    assert!(
        message.contains("results"),
        "should show the shape: {message}"
    );
}

#[tokio::test]
async fn a_slow_provider_times_out_and_is_worth_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(10))
                .set_body_json(serde_json::json!({"text": "too late"})),
        )
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    let error = vc_stt::HttpTranscriber::new("p".to_owned(), http_config(server.uri()), 200)
        .transcribe(&fixture.request())
        .await
        .expect_err("should time out");

    assert!(error.is_transient(), "{error}");
}

#[tokio::test]
async fn an_unreachable_provider_is_worth_retrying() {
    // Nothing is listening on this port.
    let fixture = Fixture::new();
    let error = vc_stt::HttpTranscriber::new(
        "p".to_owned(),
        http_config("http://127.0.0.1:1/transcribe".to_owned()),
        2_000,
    )
    .transcribe(&fixture.request())
    .await
    .expect_err("should fail");

    assert!(
        error.is_transient(),
        "a refused connection should be retried: {error}"
    );
}

#[tokio::test]
async fn a_missing_audio_file_is_reported_by_path() {
    let fixture = Fixture::new();
    let mut request = fixture.request();
    request.audio_path = PathBuf::from("/does/not/exist.wav");

    let error = vc_stt::HttpTranscriber::new(
        "p".to_owned(),
        http_config("http://127.0.0.1:9/x".to_owned()),
        2_000,
    )
    .transcribe(&request)
    .await
    .expect_err("should fail");

    assert!(error.to_string().contains("/does/not/exist.wav"), "{error}");
}

// ── the openai preset ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_openai_preset_builds_the_request_the_api_expects() {
    std::env::set_var("VC_TEST_OPENAI_KEY", "sk-test-key");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("authorization", "Bearer sk-test-key"))
        .and(body_string_contains("gpt-4o-transcribe"))
        .and(body_string_contains("WAVEfake"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "text": "hello from openai"
        })))
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    let transcript = vc_stt::OpenAiTranscriber::new(
        "openai".to_owned(),
        openai_config(format!("{}/v1", server.uri())),
        5_000,
    )
    .transcribe(&fixture.request())
    .await
    .expect("should transcribe");

    assert_eq!(transcript.text, "hello from openai");
}

#[tokio::test]
async fn a_trailing_slash_on_the_base_url_does_not_double_up() {
    std::env::set_var("VC_TEST_OPENAI_KEY", "sk-test-key");

    let server = MockServer::start().await;
    Mock::given(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ok"})))
        .mount(&server)
        .await;

    let fixture = Fixture::new();
    vc_stt::OpenAiTranscriber::new(
        "openai".to_owned(),
        openai_config(format!("{}/v1/", server.uri())),
        5_000,
    )
    .transcribe(&fixture.request())
    .await
    .expect("a trailing slash is a natural thing to write");
}

#[tokio::test]
async fn optional_openai_settings_are_sent_only_when_set() {
    std::env::set_var("VC_TEST_OPENAI_KEY", "sk-test-key");

    let server = MockServer::start().await;
    Mock::given(body_string_contains("name=\"language\""))
        .and(body_string_contains("de"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ja"})))
        .mount(&server)
        .await;

    let mut config = openai_config(format!("{}/v1", server.uri()));
    config.language = Some("de".to_owned());

    let fixture = Fixture::new();
    vc_stt::OpenAiTranscriber::new("openai".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("language should be sent when configured");
}

// ── how the key is obtained ──────────────────────────────────────────────────

#[tokio::test]
async fn an_unset_key_variable_explains_the_user_service_trap() {
    // A systemd user service does not inherit your interactive shell, which is the single
    // most likely reason this fails for someone whose key works fine in a terminal.
    std::env::remove_var("VC_TEST_MISSING_KEY");

    let mut config = openai_config("http://127.0.0.1:9/v1".to_owned());
    config.api_key = SecretRef {
        env: Some("VC_TEST_MISSING_KEY".to_owned()),
        command: None,
    };

    let fixture = Fixture::new();
    let error = vc_stt::OpenAiTranscriber::new("openai".to_owned(), config, 2_000)
        .transcribe(&fixture.request())
        .await
        .expect_err("should fail");

    let message = error.to_string();
    assert!(message.contains("VC_TEST_MISSING_KEY"), "{message}");
    assert!(
        message.contains("import-environment"),
        "should say how to fix it: {message}"
    );
}

#[tokio::test]
async fn a_key_command_is_run_and_its_output_trimmed() {
    // A password manager prints a trailing newline, and sending that in a header is a 400
    // that looks exactly like a wrong key.
    let server = MockServer::start().await;
    Mock::given(header("authorization", "Bearer sk-from-pass"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "ok"})))
        .mount(&server)
        .await;

    let mut config = openai_config(format!("{}/v1", server.uri()));
    config.api_key = SecretRef {
        env: None,
        command: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf 'sk-from-pass\\n'".to_owned(),
        ]),
    };

    let fixture = Fixture::new();
    vc_stt::OpenAiTranscriber::new("openai".to_owned(), config, 5_000)
        .transcribe(&fixture.request())
        .await
        .expect("the newline should be trimmed off");
}

#[tokio::test]
async fn a_failing_key_command_is_reported_as_configuration() {
    let mut config = openai_config("http://127.0.0.1:9/v1".to_owned());
    config.api_key = SecretRef {
        env: None,
        command: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo 'gpg: decryption failed' >&2; exit 2".to_owned(),
        ]),
    };

    let fixture = Fixture::new();
    let error = vc_stt::OpenAiTranscriber::new("openai".to_owned(), config, 2_000)
        .transcribe(&fixture.request())
        .await
        .expect_err("should fail");

    assert!(matches!(error, TranscribeError::Config(_)), "{error:?}");
    assert!(error.to_string().contains("decryption failed"), "{error}");
}

// ── the registry ─────────────────────────────────────────────────────────────

#[test]
fn every_adapter_can_now_be_built_from_configuration() {
    let loaded = vc_core::Config::from_layers(&[
        vc_core::config::Layer::new("<embedded>", vc_core::config::EMBEDDED_DEFAULT),
        vc_core::config::Layer::new("config.toml", vc_core::config::EXAMPLE_CONFIG),
    ])
    .expect("the shipped example is valid");

    let (registry, errors) = vc_stt::Registry::from_config(&loaded.config);
    assert!(errors.is_empty(), "{errors:?}");

    // The example demonstrates all three, and all three now build.
    assert_eq!(registry.get("openai").map(|t| t.kind()), Some("openai"));
    assert_eq!(registry.get("groq").map(|t| t.kind()), Some("http"));
    assert_eq!(registry.get("deepgram").map(|t| t.kind()), Some("http"));
    assert_eq!(registry.get("local").map(|t| t.kind()), Some("command"));
}
