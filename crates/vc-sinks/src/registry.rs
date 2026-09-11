//! Turning configuration into callbacks.

use std::collections::BTreeMap;
use std::sync::Arc;

use vc_core::config::{Config, SinkConfig, SinkKind};
use vc_exec::{RealRunner, Runner};

use crate::{Clipboard, CommandSink, FileSink, HttpSink, Notifier, Sink, Typist};

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("sink {name}: {message}")]
    Invalid { name: String, message: String },
    #[error("sink {name} is of type {kind}, which this build does not support yet")]
    Unsupported { name: String, kind: &'static str },
}

/// Build one callback from its configuration.
pub fn build(name: &str, config: &SinkConfig) -> Result<Arc<dyn Sink>, BuildError> {
    build_with(name, config, Arc::new(RealRunner))
}

/// As [`build`], but with the external tools driven by `runner`.
///
/// The seam exists so the clipboard, typing and notification sinks can be tested against
/// their exact command lines without `wl-copy`, `wtype` or `notify-send` being installed.
pub fn build_with(
    name: &str,
    config: &SinkConfig,
    runner: Arc<dyn Runner>,
) -> Result<Arc<dyn Sink>, BuildError> {
    let requires_text = config.needs_text();
    let timeout = config.timeout_ms;

    Ok(match &config.kind {
        SinkKind::Command(command) => Arc::new(CommandSink::new(
            name.to_owned(),
            command.clone(),
            requires_text,
            timeout,
        )),
        SinkKind::Http(http) => Arc::new(HttpSink::new(
            name.to_owned(),
            http.clone(),
            requires_text,
            timeout,
        )),
        SinkKind::Clipboard(clipboard) => Arc::new(Clipboard::new(
            name.to_owned(),
            clipboard.clone(),
            timeout,
            runner,
        )),
        SinkKind::Type(typing) => Arc::new(Typist::new(
            name.to_owned(),
            typing.clone(),
            timeout,
            runner,
        )),
        SinkKind::Notify(notify) => Arc::new(Notifier::new(
            name.to_owned(),
            notify.clone(),
            timeout,
            runner,
        )),
        SinkKind::File(file) => Arc::new(FileSink::new(name.to_owned(), file.clone())),
    })
}

/// Every callback the configuration defines, by name.
#[derive(Clone, Default)]
pub struct Registry {
    sinks: BTreeMap<String, Arc<dyn Sink>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("names", &self.sinks.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Registry {
    /// Build everything the configuration defines, reporting what could not be built.
    ///
    /// Partial results on purpose: one unbuildable callback must not stop the daemon, since
    /// every other profile still works.
    pub fn from_config(config: &Config) -> (Self, Vec<BuildError>) {
        let mut sinks = BTreeMap::new();
        let mut errors = Vec::new();

        for (name, entry) in &config.sinks {
            match build(name, entry) {
                Ok(sink) => {
                    sinks.insert(name.clone(), sink);
                }
                Err(error) => errors.push(error),
            }
        }

        (Self { sinks }, errors)
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Sink>> {
        self.sinks.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.sinks.keys()
    }
}
