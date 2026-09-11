//! The `http` sink: POST somewhere.

use std::time::Duration;

use tracing::warn;
use vc_core::config::{HttpMethod, HttpSink as Config, HttpSinkBody};

use crate::{Sink, SinkContext, SinkError};

#[derive(Debug)]
pub struct HttpSink {
    name: String,
    config: Config,
    requires_text: bool,
    timeout: Duration,
}

impl HttpSink {
    pub fn new(name: String, config: Config, requires_text: bool, timeout_ms: u64) -> Self {
        Self {
            name,
            config,
            requires_text,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

#[async_trait::async_trait]
impl Sink for HttpSink {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "http"
    }
    fn requires_text(&self) -> bool {
        self.requires_text
    }

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|error| SinkError::Config(error.to_string()))?;

        let method = match self.config.method {
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Patch => reqwest::Method::PATCH,
        };

        let mut request = client.request(method, context.tokens.expand(&self.config.url));
        for (header, value) in &self.config.headers {
            let (resolved, missing) = vc_core::tokens::expand_env(value);
            for variable in missing {
                warn!(
                    sink = self.name,
                    header,
                    variable,
                    "environment variable is not set; the header will be incomplete"
                );
            }
            request = request.header(header, resolved);
        }

        request = match &self.config.body {
            // The same JSON a `command` sink receives on stdin, so a webhook and a script can
            // be written against one shape.
            HttpSinkBody::SessionJson => request
                .header("Content-Type", "application/json")
                .body(context.record_json()),
            HttpSinkBody::Template { template } => request
                .header("Content-Type", "application/json")
                .body(context.tokens.expand(template)),
            HttpSinkBody::Multipart { audio_field, form } => {
                let audio = tokio::fs::read(&context.record.audio.path)
                    .await
                    .map_err(|error| {
                        SinkError::Failed(format!(
                            "cannot read {} to send: {error}",
                            context.record.audio.path.display()
                        ))
                    })?;
                let filename = context.record.audio.path.file_name().map_or_else(
                    || "audio.wav".to_owned(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let part = reqwest::multipart::Part::bytes(audio)
                    .file_name(filename)
                    .mime_str("audio/wav")
                    .map_err(|error| SinkError::Config(error.to_string()))?;

                let mut multipart = reqwest::multipart::Form::new().part(audio_field.clone(), part);
                for (key, value) in form {
                    multipart = multipart.text(key.clone(), context.tokens.expand(value));
                }
                request.multipart(multipart)
            }
        };

        let response = request.send().await.map_err(|error| {
            if error.is_timeout() || error.is_connect() || error.is_request() {
                // The network, not the request.
                SinkError::Transient(error.to_string())
            } else {
                SinkError::Failed(error.to_string())
            }
        })?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body: String = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        let message = format!("HTTP {}: {}", status.as_u16(), body.trim());

        match status.as_u16() {
            // Busy now, very likely not in a moment.
            429 | 500..=599 => Err(SinkError::Transient(message)),
            // A wrong token or a wrong URL will be just as wrong next time.
            401 | 403 | 404 => Err(SinkError::Config(message)),
            _ => Err(SinkError::Failed(message)),
        }
    }
}
