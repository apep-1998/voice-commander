//! The `command` transcriber: run any program.
//!
//! This is the offline path and the universal escape hatch. `whisper-cli`, `faster-whisper`,
//! a Python script, anything with a command line — no network, no API key, and nothing in
//! this repository needs to know the tool exists.

use std::path::PathBuf;
use std::time::Duration;

use tracing::debug;
use vc_core::config::{CommandTranscriber as Config, TextSource};
use vc_core::tokens::Tokens;
use vc_exec::{CommandSpec, ExecError};

use crate::{TranscribeError, TranscribeRequest, Transcriber, Transcript};

/// A transcriber that shells out.
#[derive(Debug)]
pub struct CommandTranscriber {
    name: String,
    config: Config,
    timeout: Duration,
}

impl CommandTranscriber {
    pub fn new(name: String, config: Config, timeout_ms: u64) -> Self {
        Self {
            name,
            config,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// The tokens available to this command.
    ///
    /// Fewer than a callback gets: there is no transcript yet, which is the entire point of
    /// running this.
    fn tokens(&self, request: &TranscribeRequest) -> Tokens {
        let mut tokens = Tokens::default();
        tokens.set("audio_path", request.audio_path.display().to_string());
        tokens.set("session_dir", request.session_dir.display().to_string());
        tokens.set("session_id", request.session_id.to_string());
        tokens.set("profile", request.profile.clone());
        tokens.set("duration_ms", request.duration_ms.to_string());
        tokens.set("sample_rate", request.sample_rate.to_string());
        tokens
    }
}

#[async_trait::async_trait]
impl Transcriber for CommandTranscriber {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "command"
    }

    async fn transcribe(&self, request: &TranscribeRequest) -> Result<Transcript, TranscribeError> {
        let tokens = self.tokens(request);

        for unknown in tokens.unknown_in(&self.config.cmd.join(" ")) {
            // Not fatal — the user may genuinely want a literal brace — but a typo here
            // means the program is handed `{audio_pth}` and fails for a reason that looks
            // like the program's fault rather than the config's.
            tracing::warn!(
                transcriber = self.name,
                token = unknown,
                "unknown token in cmd; it will be passed through literally"
            );
        }

        let spec = CommandSpec::new(tokens.expand_args(&self.config.cmd))
            .with_env(self.config.env.clone())
            .with_cwd(Some(request.session_dir.clone()))
            .with_timeout(self.timeout);

        debug!(transcriber = self.name, argv = ?spec.argv, "running transcriber");

        let output = vc_exec::run(&spec).await.map_err(|error| match error {
            // A missing program will still be missing next time.
            ExecError::NotFound { .. } | ExecError::Empty => {
                TranscribeError::Config(error.to_string())
            }
            other => TranscribeError::Failed(other.to_string()),
        })?;

        if output.timed_out {
            // Worth another go: a local model can be slow because something else was using
            // the machine.
            return Err(TranscribeError::Transient(format!(
                "{} {}",
                self.config.cmd.first().map_or("command", String::as_str),
                output.failure_reason()
            )));
        }
        if !output.succeeded() {
            // The program ran and decided to fail. Running it again does the same thing.
            return Err(TranscribeError::Failed(output.failure_reason()));
        }

        let text = match &self.config.text {
            TextSource::Stdout => output.stdout,
            TextSource::File { path } => {
                let path = PathBuf::from(vc_exec::command::expand_tilde(&tokens.expand(path)));
                tokio::fs::read_to_string(&path).await.map_err(|error| {
                    TranscribeError::Failed(format!(
                        "{} said it succeeded but {} is not readable: {error}",
                        self.name,
                        path.display()
                    ))
                })?
            }
        };

        Ok(Transcript {
            // Local models habitually pad their output with newlines, and a trailing newline
            // typed into a chat box sends the message.
            text: text.trim().to_owned(),
            language: None,
        })
    }
}
