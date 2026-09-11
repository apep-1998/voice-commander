//! The OpenAI transcriber, and anything that reimplements its API.
//!
//! Deliberately a thin preset over the generic HTTP adapter rather than a separate
//! implementation. Everything the two would otherwise duplicate — retry classification,
//! pointer extraction, multipart construction — stays on one code path, which also means the
//! generic adapter is exercised by every OpenAI user rather than being the lightly-tested one
//! nobody notices is broken.

use std::collections::BTreeMap;
use std::time::Duration;

use vc_core::config::{
    AudioAttach, HttpMethod, OpenAiTranscriber as Config, ResponseExtract, SecretRef,
};
use vc_exec::CommandSpec;

use crate::http::{execute, resolve_headers, HttpCall};
use crate::{TranscribeError, TranscribeRequest, Transcriber, Transcript};

#[derive(Debug)]
pub struct OpenAiTranscriber {
    name: String,
    config: Config,
    timeout: Duration,
}

impl OpenAiTranscriber {
    pub fn new(name: String, config: Config, timeout_ms: u64) -> Self {
        Self {
            name,
            config,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

#[async_trait::async_trait]
impl Transcriber for OpenAiTranscriber {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "openai"
    }

    async fn transcribe(&self, request: &TranscribeRequest) -> Result<Transcript, TranscribeError> {
        let key = resolve_secret(&self.name, &self.config.api_key).await?;

        let mut headers = resolve_headers(&self.name, &BTreeMap::new());
        headers.insert("Authorization".to_owned(), format!("Bearer {key}"));

        let mut form = BTreeMap::new();
        form.insert("model".to_owned(), self.config.model.clone());
        if let Some(language) = &self.config.language {
            form.insert("language".to_owned(), language.clone());
        }
        if let Some(prompt) = &self.config.prompt {
            form.insert("prompt".to_owned(), prompt.clone());
        }
        if let Some(temperature) = self.config.temperature {
            form.insert("temperature".to_owned(), temperature.to_string());
        }

        let call = HttpCall {
            url: format!(
                "{}/audio/transcriptions",
                self.config.base_url.trim_end_matches('/')
            ),
            method: HttpMethod::Post,
            headers,
            audio: AudioAttach::Multipart {
                field: "file".to_owned(),
            },
            form,
            json: BTreeMap::new(),
            query: BTreeMap::new(),
            response: ResponseExtract::default(),
            timeout: self.timeout,
        };

        execute(&self.name, &call, &request.audio_path).await
    }
}

/// Fetch the API key from wherever the user put it.
///
/// There is no variant that holds the key itself, and that is the point: configuration files
/// get committed to dotfile repositories and pasted into bug reports.
pub(crate) async fn resolve_secret(
    name: &str,
    secret: &SecretRef,
) -> Result<String, TranscribeError> {
    if let Some(variable) = &secret.env {
        return match std::env::var(variable) {
            Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_owned()),
            Ok(_) => Err(TranscribeError::Config(format!(
                "{name}: ${variable} is set but empty"
            ))),
            Err(_) => Err(TranscribeError::Config(format!(
                "{name}: ${variable} is not set in the daemon's environment\n\
                 (a user service does not inherit your shell; use \
                 `systemctl --user import-environment {variable}`, a drop-in, or \
                 ~/.config/environment.d/)"
            ))),
        };
    }

    if let Some(argv) = &secret.command {
        let spec = CommandSpec::new(argv.clone()).with_timeout(Duration::from_secs(15));
        let output = vc_exec::run(&spec)
            .await
            .map_err(|error| TranscribeError::Config(format!("{name}: {error}")))?;

        if !output.succeeded() {
            return Err(TranscribeError::Config(format!(
                "{name}: the api_key command failed: {}",
                output.failure_reason()
            )));
        }
        // A password manager prints a trailing newline; sending it in a header is a 400 that
        // looks like a wrong key.
        let key = output.stdout.trim();
        if key.is_empty() {
            return Err(TranscribeError::Config(format!(
                "{name}: the api_key command printed nothing"
            )));
        }
        return Ok(key.to_owned());
    }

    Err(TranscribeError::Config(format!(
        "{name}: api_key has neither `env` nor `command`"
    )))
}
