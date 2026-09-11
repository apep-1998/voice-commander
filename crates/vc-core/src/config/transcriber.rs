//! Speech-to-text provider configuration.
//!
//! Three adapters cover the field. `openai` is a convenience wrapper over the shape most
//! providers copied; `http` is the same machinery with every knob exposed, which is what
//! makes a new provider a config change; `command` shells out, which covers every local
//! model and anything else with a CLI.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::adapter::{
    AudioAttach, Headers, HttpMethod, ResponseExtract, RetryConfig, SecretRef, TextSource,
};

/// One named entry under `[transcribers.*]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriberConfig {
    #[serde(flatten)]
    pub kind: TranscriberKind,
    /// Give up on a single attempt after this long.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub retry: RetryConfig,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriberKind {
    /// OpenAI's `/v1/audio/transcriptions`, and anything that reimplements it.
    Openai(OpenAiTranscriber),
    /// Any HTTP API, described field by field.
    Http(HttpTranscriber),
    /// Any executable. The offline path.
    Command(CommandTranscriber),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAiTranscriber {
    #[serde(default = "default_openai_model")]
    pub model: String,
    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
    pub api_key: SecretRef,
    /// ISO-639-1 hint. Omitting it lets the model detect the language, which costs a little
    /// accuracy but handles code-switching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Biases the decoder — useful for names and jargon it would otherwise mangle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

fn default_openai_model() -> String {
    "gpt-4o-transcribe".to_owned()
}

fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpTranscriber {
    pub url: String,
    #[serde(default)]
    pub method: HttpMethod,
    #[serde(default)]
    pub headers: Headers,
    pub audio: AudioAttach,
    /// Extra multipart fields, for `audio.how = "multipart"`.
    #[serde(default)]
    pub form: BTreeMap<String, String>,
    /// Extra top-level JSON keys, for `audio.how = "base64_json"`.
    #[serde(default)]
    pub json: BTreeMap<String, String>,
    /// Query string parameters, which is where several providers put their options.
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default)]
    pub response: ResponseExtract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandTranscriber {
    /// Argv, not a shell string. Tokens such as `{audio_path}` are substituted per argument,
    /// so a path containing spaces cannot turn into two arguments.
    pub cmd: Vec<String>,
    pub text: TextSource,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
