//! Callback configuration — where a finished recording gets delivered.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::adapter::{Headers, HttpMethod, RetryConfig};

/// What a failing sink does to the rest of the fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Record the failure and carry on with the other sinks. The default, because one
    /// unreachable webhook should not stop the transcript reaching the clipboard.
    #[default]
    Ignore,
    /// Mark the whole session failed. For sinks whose success is the point of the profile.
    FailSession,
}

/// Whether a profile's sinks run together or in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkMode {
    /// All at once. Total latency is the slowest sink rather than their sum.
    #[default]
    Parallel,
    /// One after another, in the order listed. For when a later sink depends on what an
    /// earlier one did.
    Sequential,
}

/// One named entry under `[sinks.*]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkConfig {
    #[serde(flatten)]
    pub kind: SinkKind,
    /// Skip this sink when the session produced no transcript. Defaults to whatever the
    /// sink kind needs: a `clipboard` sink is pointless without text, a `command` sink
    /// handed a `.wav` path is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_text: Option<bool>,
    #[serde(default)]
    pub on_error: OnError,
    #[serde(default = "default_sink_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub retry: RetryConfig,
}

fn default_sink_timeout_ms() -> u64 {
    60_000
}

impl SinkConfig {
    /// Whether this sink should be skipped when there is no transcript.
    pub fn needs_text(&self) -> bool {
        self.requires_text
            .unwrap_or_else(|| self.kind.needs_text_by_default())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SinkKind {
    /// Run a program. The general escape hatch, and the reason this project exists.
    Command(CommandSink),
    /// POST somewhere.
    Http(HttpSink),
    /// Copy the transcript to the Wayland clipboard.
    Clipboard(ClipboardSink),
    /// Type the transcript into whatever window has focus.
    Type(TypeSink),
    /// Raise a desktop notification.
    Notify(NotifySink),
    /// Append to a file.
    File(FileSink),
}

impl SinkKind {
    fn needs_text_by_default(&self) -> bool {
        match self {
            // These have nothing to do without a transcript.
            Self::Clipboard(_) | Self::Type(_) | Self::File(_) => true,
            // These are perfectly useful with only an audio file.
            Self::Command(_) | Self::Http(_) | Self::Notify(_) => false,
        }
    }

    /// The `type` value that produced this variant, for diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Command(_) => "command",
            Self::Http(_) => "http",
            Self::Clipboard(_) => "clipboard",
            Self::Type(_) => "type",
            Self::Notify(_) => "notify",
            Self::File(_) => "file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSink {
    /// Argv with `{token}` substitution applied per argument.
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Write the session metadata as JSON to the child's standard input. A shell script can
    /// ignore it and read `$1`; anything more involved can parse it.
    #[serde(default = "crate::config::truth")]
    pub stdin_json: bool,
    /// Working directory for the child. Defaults to the session directory, so a script can
    /// write alongside the recording without knowing where that is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSink {
    pub url: String,
    #[serde(default)]
    pub method: HttpMethod,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default)]
    pub body: HttpSinkBody,
}

/// What an HTTP sink sends.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HttpSinkBody {
    /// The full session metadata as JSON.
    #[default]
    SessionJson,
    /// A JSON body built from a template string with `{token}` substitution.
    Template { template: String },
    /// `multipart/form-data` carrying the audio file, plus any extra fields.
    Multipart {
        #[serde(default = "default_audio_field")]
        audio_field: String,
        #[serde(default)]
        form: BTreeMap<String, String>,
    },
}

fn default_audio_field() -> String {
    "file".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClipboardSink {
    /// Also put the text on the primary selection (middle-click paste).
    #[serde(default)]
    pub primary: bool,
}

/// Which tool injects synthetic keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeTool {
    /// Prefer `wtype`, fall back to `ydotool`.
    #[default]
    Auto,
    Wtype,
    Ydotool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TypeSink {
    #[serde(default)]
    pub tool: TypeTool,
    /// Delay between keystrokes. Some applications drop input typed faster than a human can.
    #[serde(default)]
    pub key_delay_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifySink {
    #[serde(default = "default_notify_summary")]
    pub summary: String,
    #[serde(default = "default_notify_body")]
    pub body: String,
    #[serde(default)]
    pub urgency: Urgency,
    #[serde(default = "default_notify_timeout_ms")]
    pub expire_ms: u32,
}

fn default_notify_summary() -> String {
    "voice-commander".to_owned()
}

fn default_notify_body() -> String {
    "{text}".to_owned()
}

fn default_notify_timeout_ms() -> u32 {
    5_000
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSink {
    /// Destination path. Supports `{token}` substitution, so a per-day log is just
    /// `~/notes/{date}.md`.
    pub path: String,
    /// What to write, with `{token}` substitution.
    #[serde(default = "default_file_template")]
    pub template: String,
    /// Append rather than truncate.
    #[serde(default = "crate::config::truth")]
    pub append: bool,
}

fn default_file_template() -> String {
    "{started_at} {text}\n".to_owned()
}
