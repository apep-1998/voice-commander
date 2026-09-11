//! Speech-to-text for voice-commander.
//!
//! Transcription is optional and pluggable: a profile may name a transcriber or omit one
//! entirely, and the adapters are driven from configuration so that adding a provider is a
//! config change rather than a code change.
//!
//! The trait is what everything downstream sees, so a built-in adapter and a
//! config-described one are indistinguishable to the pipeline.

pub mod command;
pub mod registry;

use std::path::PathBuf;

use vc_core::session::SessionId;

pub use command::CommandTranscriber;
pub use registry::{build, BuildError, Registry};

/// What a transcriber is asked to do.
#[derive(Debug, Clone)]
pub struct TranscribeRequest {
    pub audio_path: PathBuf,
    pub session_dir: PathBuf,
    pub session_id: SessionId,
    pub profile: String,
    pub sample_rate: u32,
    pub duration_ms: u64,
}

/// What it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// What the provider detected or was told, when it says.
    pub language: Option<String>,
}

/// Why transcription failed.
#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("{0}")]
    Failed(String),
    /// Failed in a way a second attempt might survive — a timeout, a 5xx, a refused
    /// connection. Distinguished because retrying anything else just does the wrong thing
    /// twice.
    #[error("{0}")]
    Transient(String),
    #[error("configuration: {0}")]
    Config(String),
}

impl TranscribeError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Something that turns audio into text.
#[async_trait::async_trait]
pub trait Transcriber: Send + Sync {
    /// The name of the `[transcribers.*]` entry this was built from.
    fn name(&self) -> &str;

    /// The adapter kind, for diagnostics and for the event stream.
    fn kind(&self) -> &'static str;

    async fn transcribe(&self, request: &TranscribeRequest) -> Result<Transcript, TranscribeError>;
}
