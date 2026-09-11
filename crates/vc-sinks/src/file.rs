//! The `file` sink: append the transcript to a file.
//!
//! The voice-journal case. Both the path and the line are templates, so
//! `~/notes/voice/{date}.md` gives a file per day with no cron job and no wrapper script.

use std::path::PathBuf;

use vc_core::config::FileSink as Config;

use crate::{Sink, SinkContext, SinkError};

#[derive(Debug)]
pub struct FileSink {
    name: String,
    config: Config,
}

impl FileSink {
    pub fn new(name: String, config: Config) -> Self {
        Self { name, config }
    }
}

#[async_trait::async_trait]
impl Sink for FileSink {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> &'static str {
        "file"
    }
    fn requires_text(&self) -> bool {
        true
    }

    async fn deliver(&self, context: &SinkContext) -> Result<(), SinkError> {
        let path = PathBuf::from(vc_exec::command::expand_tilde(
            &context.tokens.expand(&self.config.path),
        ));
        let line = context.tokens.expand(&self.config.template);

        // Create the directory rather than failing: `~/notes/voice/{date}.md` on the first
        // day of use points somewhere that does not exist yet, and telling the user to go and
        // mkdir it is a poor first impression.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    SinkError::Failed(format!("creating {}: {error}", parent.display()))
                })?;
            }
        }

        if self.config.append {
            use tokio::io::AsyncWriteExt;

            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await
                .map_err(|error| {
                    SinkError::Failed(format!("opening {}: {error}", path.display()))
                })?;

            file.write_all(line.as_bytes()).await.map_err(|error| {
                SinkError::Failed(format!("appending to {}: {error}", path.display()))
            })?;
            // Flushed explicitly: a journal entry that exists only in a buffer when the
            // daemon exits is a journal entry the user lost.
            file.flush()
                .await
                .map_err(|error| SinkError::Failed(format!("flushing {}: {error}", path.display())))
        } else {
            tokio::fs::write(&path, line.as_bytes())
                .await
                .map_err(|error| SinkError::Failed(format!("writing {}: {error}", path.display())))
        }
    }
}
