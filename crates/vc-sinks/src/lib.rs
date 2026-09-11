//! Callbacks: where a finished recording is delivered.
//!
//! A sink is anything a recording can be handed to — a program, a webhook, the clipboard,
//! the focused window. A profile fans out to as many as it likes, and they are independent of
//! each other by default, so the total wait is the slowest one rather than the sum.
//!
//! The design constraint that shapes everything here: **one failing callback must not take
//! the others down**. An unreachable webhook should not stop the transcript reaching the
//! clipboard, and the user should be told which one failed and why.

pub mod command;
pub mod fanout;
pub mod registry;

use std::path::PathBuf;

use vc_core::session::SessionRecord;
use vc_core::tokens::Tokens;

pub use command::CommandSink;
pub use fanout::{run, tally, Planned, Progress};
pub use registry::{build, BuildError, Registry};

/// Everything a callback is given.
#[derive(Debug, Clone)]
pub struct SinkContext {
    /// The full session metadata, as written to `session.json`.
    pub record: SessionRecord,
    /// The transcript, when the profile produced one.
    pub text: Option<String>,
    pub text_path: Option<PathBuf>,
    /// Substitution values, derived from the above.
    pub tokens: Tokens,
}

impl SinkContext {
    pub fn new(record: SessionRecord, text: Option<String>, text_path: Option<PathBuf>) -> Self {
        let tokens = Tokens::for_session(&record, text.as_deref(), text_path.as_deref());
        Self {
            record,
            text,
            text_path,
            tokens,
        }
    }

    /// The session metadata as JSON, for a callback that wants it on standard input.
    pub fn record_json(&self) -> String {
        serde_json::to_string_pretty(&self.record).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// Why a callback failed.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("{0}")]
    Failed(String),
    /// Failed in a way another attempt might survive.
    #[error("{0}")]
    Transient(String),
    #[error("configuration: {0}")]
    Config(String),
}

impl SinkError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Something a finished recording can be delivered to.
#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    /// The name of the `[sinks.*]` entry this was built from.
    fn name(&self) -> &str;

    /// The adapter kind, so an indicator can show an icon without reading the config.
    fn kind(&self) -> &'static str;

    /// Whether this should be skipped when the session produced no transcript.
    fn requires_text(&self) -> bool;

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError>;
}
