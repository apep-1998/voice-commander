//! Pieces shared by the configuration of every outbound adapter — the HTTP-shaped
//! transcribers and sinks, and the ones that shell out to a command.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Retry policy for an operation that talks to something outside this process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Total attempts, including the first. `1` disables retrying.
    pub attempts: u32,
    /// Delay before the second attempt; doubled for each attempt after that.
    pub backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: 1,
            backoff_ms: 500,
        }
    }
}

/// How the audio is attached to an outbound HTTP request.
///
/// Providers disagree about this more than about anything else, which is exactly why it is
/// configuration rather than three hardcoded implementations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum AudioAttach {
    /// `multipart/form-data` with the audio under `field`. What OpenAI, Groq and most
    /// OpenAI-compatible servers expect.
    Multipart {
        #[serde(default = "default_audio_field")]
        field: String,
    },
    /// The raw audio bytes as the entire request body. What Deepgram expects.
    RawBody,
    /// A JSON body with the audio base64-encoded into `field`.
    Base64Json {
        #[serde(default = "default_audio_field")]
        field: String,
    },
}

fn default_audio_field() -> String {
    "file".to_owned()
}

/// How the response body maps to a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Parse as JSON and follow `text_pointer` to the transcript.
    Json,
    /// The whole response body is the transcript.
    Text,
}

/// Where in the response the transcript lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseExtract {
    pub format: ResponseFormat,
    /// An RFC 6901 JSON pointer, e.g. `/text` or `/results/channels/0/alternatives/0/transcript`.
    /// Ignored when `format = "text"`.
    #[serde(default = "default_text_pointer")]
    pub text_pointer: String,
}

fn default_text_pointer() -> String {
    "/text".to_owned()
}

impl Default for ResponseExtract {
    fn default() -> Self {
        Self {
            format: ResponseFormat::Json,
            text_pointer: default_text_pointer(),
        }
    }
}

/// Where a secret comes from.
///
/// Note the absence of a variant holding the secret itself. Configuration files get copied
/// into dotfile repositories and pasted into issue reports; an API key does not belong in
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    /// Read the secret from this environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Run this command and use its trimmed standard output, e.g. `["pass", "show", "openai"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

/// Where the text produced by a `command` transcriber is read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum TextSource {
    /// The command printed the transcript to standard output.
    Stdout,
    /// The command wrote the transcript to this path. Supports the same substitution tokens
    /// as the command itself, so `{session_dir}/out.txt` works.
    File { path: String },
}

/// An HTTP method, restricted to the ones that carry a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    #[default]
    Post,
    Put,
    Patch,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
        }
    }
}

/// Header name/value pairs. Values may contain `${VAR}` references, expanded from the
/// environment when the request is built rather than when the config is loaded — so
/// rotating a key does not mean reloading the daemon.
pub type Headers = BTreeMap<String, String>;
