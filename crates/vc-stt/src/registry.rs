//! Turning configuration into transcribers.

use std::collections::BTreeMap;
use std::sync::Arc;

use vc_core::config::{Config, TranscriberConfig, TranscriberKind};

use crate::{CommandTranscriber, Transcriber};

/// Why a transcriber could not be built.
///
/// Separate from the configuration validator: that one checks what can be known by reading
/// the file, this one covers what can only be found out by trying.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("transcriber {name}: {message}")]
    Invalid { name: String, message: String },
    #[error("transcriber {name} is of type {kind}, which this build does not support")]
    Unsupported { name: String, kind: &'static str },
}

/// Build one transcriber from its configuration.
pub fn build(name: &str, config: &TranscriberConfig) -> Result<Arc<dyn Transcriber>, BuildError> {
    match &config.kind {
        TranscriberKind::Command(command) => Ok(Arc::new(CommandTranscriber::new(
            name.to_owned(),
            command.clone(),
            config.timeout_ms,
        ))),
        // The HTTP adapters arrive in the next PR. Naming them explicitly means a user who
        // configures one now is told so, rather than finding the profile silently producing
        // no transcript.
        TranscriberKind::Openai(_) => Err(BuildError::Unsupported {
            name: name.to_owned(),
            kind: "openai",
        }),
        TranscriberKind::Http(_) => Err(BuildError::Unsupported {
            name: name.to_owned(),
            kind: "http",
        }),
    }
}

/// Every transcriber the configuration defines, by name.
#[derive(Clone, Default)]
pub struct Registry {
    transcribers: BTreeMap<String, Arc<dyn Transcriber>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("names", &self.transcribers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Registry {
    /// Build everything the configuration defines, reporting what could not be built.
    ///
    /// Returns partial results on purpose. One unbuildable transcriber should not stop the
    /// daemon: the other profiles still work, and the user finds out from a log line and
    /// from the sink being skipped rather than from nothing starting at all.
    pub fn from_config(config: &Config) -> (Self, Vec<BuildError>) {
        let mut transcribers = BTreeMap::new();
        let mut errors = Vec::new();

        for (name, entry) in &config.transcribers {
            match build(name, entry) {
                Ok(transcriber) => {
                    transcribers.insert(name.clone(), transcriber);
                }
                Err(error) => errors.push(error),
            }
        }

        (Self { transcribers }, errors)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Transcriber>> {
        self.transcribers.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.transcribers.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.transcribers.is_empty()
    }
}
