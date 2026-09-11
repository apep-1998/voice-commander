//! The HTTP transcribers.
//!
//! One request builder serves both adapters, because `openai` is not a different mechanism —
//! it is a preset of the generic one with the URL, the multipart field and the JSON pointer
//! already filled in. Keeping them on one code path means the generic adapter is exercised
//! by every OpenAI user, rather than being the lightly-tested one nobody notices is broken.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use tracing::{debug, warn};
use vc_core::config::{AudioAttach, HttpMethod, ResponseExtract, ResponseFormat};

use crate::{TranscribeError, Transcript};

/// A fully resolved request: secrets substituted, tokens expanded, ready to send.
#[derive(Debug, Clone)]
pub(crate) struct HttpCall {
    pub url: String,
    pub method: HttpMethod,
    /// Header values with `${VAR}` already expanded.
    pub headers: BTreeMap<String, String>,
    pub audio: AudioAttach,
    pub form: BTreeMap<String, String>,
    pub json: BTreeMap<String, String>,
    pub query: BTreeMap<String, String>,
    pub response: ResponseExtract,
    pub timeout: Duration,
}

/// Send the request and pull the transcript out of the reply.
pub(crate) async fn execute(
    name: &str,
    call: &HttpCall,
    audio_path: &Path,
) -> Result<Transcript, TranscribeError> {
    let audio = tokio::fs::read(audio_path).await.map_err(|error| {
        TranscribeError::Failed(format!(
            "cannot read {} to send: {error}",
            audio_path.display()
        ))
    })?;

    let client = reqwest::Client::builder()
        .timeout(call.timeout)
        .build()
        .map_err(|error| TranscribeError::Config(error.to_string()))?;

    let method = match call.method {
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Patch => reqwest::Method::PATCH,
    };

    let mut request = client.request(method, &call.url);
    if !call.query.is_empty() {
        request = request.query(&call.query.iter().collect::<Vec<_>>());
    }
    for (header, value) in &call.headers {
        request = request.header(header, value);
    }

    let filename = audio_path.file_name().map_or_else(
        || "audio.wav".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );

    request = match &call.audio {
        AudioAttach::Multipart { field } => {
            let part = reqwest::multipart::Part::bytes(audio)
                .file_name(filename)
                // Providers reject a part with no content type, or silently treat it as
                // text. This is the single most common reason a hand-written `http`
                // transcriber returns 400.
                .mime_str("audio/wav")
                .map_err(|error| TranscribeError::Config(error.to_string()))?;
            let mut form = reqwest::multipart::Form::new().part(field.clone(), part);
            for (key, value) in &call.form {
                form = form.text(key.clone(), value.clone());
            }
            request.multipart(form)
        }
        AudioAttach::RawBody => request.body(audio),
        AudioAttach::Base64Json { field } => {
            let mut body = serde_json::Map::new();
            body.insert(
                field.clone(),
                serde_json::Value::String(encode_base64(&audio)),
            );
            for (key, value) in &call.json {
                body.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
            request.json(&serde_json::Value::Object(body))
        }
    };

    debug!(transcriber = name, url = call.url, "sending audio");

    let response = request
        .send()
        .await
        .map_err(|error| classify_send(&error))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(classify_status(name, status.as_u16(), &body));
    }

    extract(name, &call.response, &body)
}

/// Turn a transport failure into a verdict about retrying.
fn classify_send(error: &reqwest::Error) -> TranscribeError {
    if error.is_timeout() || error.is_connect() || error.is_request() {
        // The network, not the request. Worth another go.
        TranscribeError::Transient(error.to_string())
    } else {
        TranscribeError::Failed(error.to_string())
    }
}

/// Turn a status code into a verdict about retrying.
///
/// The distinction that matters: a 401 means the key is wrong and will still be wrong in
/// 500ms, while a 429 or a 503 means the provider is busy and very likely will not be.
fn classify_status(name: &str, status: u16, body: &str) -> TranscribeError {
    // Providers put their explanation in different places; try the common ones before
    // falling back to the raw body, truncated so a stray HTML error page does not fill a log.
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            ["/error/message", "/message", "/err_msg", "/detail"]
                .iter()
                .find_map(|pointer| {
                    value
                        .pointer(pointer)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
        })
        .unwrap_or_else(|| body.chars().take(200).collect());

    let message = format!("{name}: HTTP {status}: {detail}");
    match status {
        429 | 500..=599 => TranscribeError::Transient(message),
        401 | 403 => TranscribeError::Config(message),
        _ => TranscribeError::Failed(message),
    }
}

/// Pull the transcript out of the response body.
fn extract(
    name: &str,
    response: &ResponseExtract,
    body: &str,
) -> Result<Transcript, TranscribeError> {
    match response.format {
        ResponseFormat::Text => Ok(Transcript {
            text: body.trim().to_owned(),
            language: None,
        }),
        ResponseFormat::Json => {
            let value: serde_json::Value = serde_json::from_str(body).map_err(|error| {
                TranscribeError::Failed(format!(
                    "{name} returned something that is not JSON: {error}"
                ))
            })?;

            let text = value
                .pointer(&response.text_pointer)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    // Naming the pointer and showing the shape is the difference between a
                    // one-line config fix and an afternoon with curl.
                    TranscribeError::Config(format!(
                        "{name}: no text at {:?} in the response; it looks like {}",
                        response.text_pointer,
                        shape(&value)
                    ))
                })?;

            Ok(Transcript {
                text: text.trim().to_owned(),
                language: value
                    .pointer("/language")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            })
        }
    }
}

/// A compact description of a JSON value's structure, for an error message.
fn shape(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).take(8).collect();
            format!("an object with keys: {}", keys.join(", "))
        }
        serde_json::Value::Array(items) => format!("an array of {} items", items.len()),
        other => format!("a {}", type_name(other)),
    }
}

fn type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Base64, without pulling in a crate for twenty lines.
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let packed = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(packed >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(packed >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(packed >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(packed & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Expand `${VAR}` in every header value, warning about any that is not set.
pub(crate) fn resolve_headers(
    name: &str,
    headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(header, value)| {
            let (resolved, missing) = vc_core::tokens::expand_env(value);
            for variable in missing {
                warn!(
                    transcriber = name,
                    header,
                    variable,
                    "environment variable is not set; the header will be incomplete"
                );
            }
            (header.clone(), resolved)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_bytes_that_are_not_text() {
        // A WAV file is not UTF-8, and every byte value has to survive. The expected values
        // come from Python's `base64.b64encode(bytes(range(256)))` rather than from this
        // implementation, so they cannot agree with a bug in it.
        let bytes: Vec<u8> = (0u8..=255).collect();
        let encoded = encode_base64(&bytes);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"));
        assert!(
            encoded.ends_with("/P3+/w=="),
            "{}",
            &encoded[encoded.len() - 8..]
        );
    }

    #[test]
    fn a_missing_pointer_says_what_the_response_actually_looked_like() {
        // The difference between a one-line config fix and an afternoon with curl.
        let error = extract(
            "groq",
            &ResponseExtract {
                format: ResponseFormat::Json,
                text_pointer: "/text".to_owned(),
            },
            r#"{"results": {"transcript": "hello"}}"#,
        )
        .expect_err("should fail");

        let message = error.to_string();
        assert!(message.contains("/text"), "{message}");
        assert!(
            message.contains("results"),
            "should show the shape: {message}"
        );
    }

    #[test]
    fn a_plain_text_response_is_taken_whole() {
        let transcript = extract(
            "local",
            &ResponseExtract {
                format: ResponseFormat::Text,
                text_pointer: String::new(),
            },
            "  hello there\n",
        )
        .expect("should extract");
        assert_eq!(transcript.text, "hello there");
    }

    #[test]
    fn a_nested_pointer_is_followed() {
        let transcript = extract(
            "deepgram",
            &ResponseExtract {
                format: ResponseFormat::Json,
                text_pointer: "/results/channels/0/alternatives/0/transcript".to_owned(),
            },
            r#"{"results":{"channels":[{"alternatives":[{"transcript":"nested"}]}]}}"#,
        )
        .expect("should extract");
        assert_eq!(transcript.text, "nested");
    }

    #[test]
    fn a_reported_language_is_kept() {
        let transcript = extract(
            "openai",
            &ResponseExtract::default(),
            r#"{"text":"hallo","language":"de"}"#,
        )
        .expect("should extract");
        assert_eq!(transcript.language.as_deref(), Some("de"));
    }

    #[test]
    fn an_unauthorized_response_is_a_configuration_problem_not_a_transient_one() {
        // A wrong key will still be wrong in 500ms; retrying it wastes the user's time and
        // can trip rate limiting on top.
        let error = classify_status("openai", 401, r#"{"error":{"message":"Invalid API key"}}"#);
        assert!(matches!(error, TranscribeError::Config(_)), "{error:?}");
        assert!(error.to_string().contains("Invalid API key"), "{error}");
    }

    #[test]
    fn rate_limiting_and_server_errors_are_worth_retrying() {
        for status in [429, 500, 502, 503] {
            let error = classify_status("groq", status, "{}");
            assert!(error.is_transient(), "HTTP {status} should be retried");
        }
    }

    #[test]
    fn a_bad_request_is_not_retried() {
        // Sending the same malformed request again produces the same 400.
        let error = classify_status("groq", 400, r#"{"error":{"message":"bad model"}}"#);
        assert!(!error.is_transient(), "{error:?}");
        assert!(error.to_string().contains("bad model"), "{error}");
    }

    #[test]
    fn a_non_json_error_page_is_truncated_rather_than_logged_whole() {
        let html = format!("<html>{}</html>", "x".repeat(5_000));
        let message = classify_status("provider", 502, &html).to_string();
        assert!(
            message.len() < 300,
            "error was {} characters",
            message.len()
        );
    }
}

/// The generic HTTP transcriber: any provider, described field by field.
///
/// This is what makes "add a provider without touching the code" true. Deepgram, Groq,
/// ElevenLabs, Azure and a self-hosted whisper.cpp server are all this adapter with
/// different values.
#[derive(Debug)]
pub struct HttpTranscriber {
    name: String,
    config: vc_core::config::HttpTranscriber,
    timeout: Duration,
}

impl HttpTranscriber {
    pub fn new(name: String, config: vc_core::config::HttpTranscriber, timeout_ms: u64) -> Self {
        Self {
            name,
            config,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

#[async_trait::async_trait]
impl crate::Transcriber for HttpTranscriber {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "http"
    }

    async fn transcribe(
        &self,
        request: &crate::TranscribeRequest,
    ) -> Result<Transcript, TranscribeError> {
        let call = HttpCall {
            url: self.config.url.clone(),
            method: self.config.method,
            headers: resolve_headers(&self.name, &self.config.headers),
            audio: self.config.audio.clone(),
            form: self.config.form.clone(),
            json: self.config.json.clone(),
            query: self.config.query.clone(),
            response: self.config.response.clone(),
            timeout: self.timeout,
        };
        execute(&self.name, &call, &request.audio_path).await
    }
}
