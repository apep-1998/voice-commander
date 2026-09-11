//! Configuration: the vocabulary the whole tool is described in.
//!
//! A configuration is assembled from layers — an embedded baseline, the user's
//! `config.toml`, then any `conf.d/*.toml` drop-ins — deep-merged in that order. Profiles
//! then inherit from `[defaults]`, so a profile only states what it does differently.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let loaded = vc_core::config::Config::load_default()?;
//! for warning in &loaded.warnings {
//!     eprintln!("warning: {warning}");
//! }
//! let profile = loaded.config.profiles.get("default").expect("baseline profile");
//! println!("{:?}", profile.capture.mode);
//! # Ok(())
//! # }
//! ```

mod adapter;
mod capture;
mod error;
mod merge;
mod profile;
mod runtime;
mod session;
mod sink;
mod transcriber;
mod validate;

pub use adapter::{
    AudioAttach, Headers, HttpMethod, ResponseExtract, ResponseFormat, RetryConfig, SecretRef,
    TextSource,
};
pub use capture::{CaptureConfig, CaptureMode, DeviceSelector};
pub use error::{ConfigError, Issue};
pub use profile::Profile;
pub use runtime::{
    AudioConfig, AudioFormat, CommandPresenter, DaemonConfig, FeedbackConfig, LevelsConfig,
    NotifyPresenter, PresenterConfig, PresenterKind, SocketPresenter, StorageConfig,
};
pub use session::{ContinuationConfig, GapMode, SessionConfig, TriggerMode};
pub use sink::{
    ClipboardSink, CommandSink, FileSink, HttpSink, HttpSinkBody, NotifySink, OnError, SinkConfig,
    SinkKind, SinkMode, TypeSink, TypeTool, Urgency,
};
pub use transcriber::{
    CommandTranscriber, HttpTranscriber, OpenAiTranscriber, TranscriberConfig, TranscriberKind,
};
pub use validate::Report;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use toml::Table;

/// The baseline every configuration is layered on top of.
///
/// Shipping this compiled in means a fresh install works before the user has written
/// anything, and that every key has exactly one documented default rather than one in the
/// docs and another in a `#[serde(default)]`.
pub const EMBEDDED_DEFAULT: &str = include_str!("../../../../config/default.toml");

/// A heavily commented starting point, written out by `voice-commander config init`.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../../config/example.toml");

/// Key prefixes whose children are arbitrary by design, and so cannot be checked against the
/// schema. HTTP headers, environment variables and provider-specific form fields all live
/// here.
const FREE_FORM_PREFIXES: &[&str] = &["__free_form_never_matches"];

pub(crate) fn truth() -> bool {
    true
}

/// Settings every profile inherits unless it says otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    pub capture: CaptureConfig,
    pub session: SessionConfig,
    pub continuation: ContinuationConfig,
}

/// A complete, resolved configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_daemon")]
    pub daemon: DaemonConfig,
    pub audio: AudioConfig,
    pub levels: LevelsConfig,
    pub storage: StorageConfig,
    pub feedback: FeedbackConfig,
    pub defaults: Defaults,
    #[serde(default)]
    pub transcribers: BTreeMap<String, TranscriberConfig>,
    #[serde(default)]
    pub sinks: BTreeMap<String, SinkConfig>,
    #[serde(default)]
    pub presenters: BTreeMap<String, PresenterConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

fn default_daemon() -> DaemonConfig {
    DaemonConfig {
        socket_path: None,
        autostart: false,
        log_level: "info".to_owned(),
    }
}

/// One source of configuration text, labelled so diagnostics can say where a problem came
/// from.
#[derive(Debug, Clone)]
pub struct Layer {
    pub source: String,
    pub text: String,
}

impl Layer {
    pub fn new(source: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            text: text.into(),
        }
    }
}

/// A configuration plus anything questionable noticed while loading it.
///
/// Warnings are returned rather than logged so that `config check` can print them, the
/// daemon can log them, and tests can assert on them — all from the same code path.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<Issue>,
}

impl Config {
    /// Load from the standard locations: the embedded baseline, then
    /// `$XDG_CONFIG_HOME/voice-commander/config.toml`, then its `conf.d/*.toml`.
    pub fn load_default() -> Result<Loaded, ConfigError> {
        Self::load_from_dir(&crate::paths::config_dir())
    }

    /// Load as [`Config::load_default`] does, but rooted at `dir`. Both the daemon's real
    /// path and the tests go through here.
    pub fn load_from_dir(dir: &Path) -> Result<Loaded, ConfigError> {
        let mut layers = vec![Layer::new("<embedded default>", EMBEDDED_DEFAULT)];

        let main = dir.join("config.toml");
        if main.is_file() {
            layers.push(Layer::new(main.display().to_string(), read(&main)?));
        }

        // Drop-ins apply in filename order, which is why the convention is to number them.
        let conf_d = dir.join("conf.d");
        if conf_d.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&conf_d)
                .map_err(|source| ConfigError::Io {
                    path: conf_d.clone(),
                    source,
                })?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            entries.sort();
            for path in entries {
                layers.push(Layer::new(path.display().to_string(), read(&path)?));
            }
        }

        Self::from_layers(&layers)
    }

    /// Merge, resolve and validate an explicit list of layers.
    pub fn from_layers(layers: &[Layer]) -> Result<Loaded, ConfigError> {
        let mut document = Table::new();
        for layer in layers {
            let parsed: Table =
                layer
                    .text
                    .parse()
                    .map_err(|error: toml::de::Error| ConfigError::Syntax {
                        source_name: layer.source.clone(),
                        message: error.to_string(),
                    })?;
            merge::merge_into(&mut document, parsed);
        }

        // Resolve inheritance while everything is still untyped. Afterwards each profile
        // carries a complete `capture`/`session`/`continuation` table of its own, and the
        // typed structs need no `Option`s to express "inherited".
        let written = document.clone();
        inherit_defaults(&mut document);

        let config: Config =
            document
                .try_into()
                .map_err(|error: toml::de::Error| ConfigError::Invalid {
                    issues: vec![Issue::new("<root>", error.to_string())],
                })?;

        let mut report = validate::check(&config);
        report.errors.extend(unknown_key_issues(&config, &written));

        if report.errors.is_empty() {
            Ok(Loaded {
                config,
                warnings: report.warnings,
            })
        } else {
            Err(ConfigError::Invalid {
                issues: report.errors,
            })
        }
    }
}

/// Give every profile its own complete copy of the inheritable sections.
fn inherit_defaults(document: &mut Table) {
    let Some(defaults) = document
        .get("defaults")
        .and_then(toml::Value::as_table)
        .cloned()
    else {
        return;
    };
    let Some(profiles) = document
        .get_mut("profiles")
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };

    let names: Vec<String> = profiles.keys().cloned().collect();
    for name in names {
        let Some(profile) = profiles.get_mut(&name).and_then(toml::Value::as_table_mut) else {
            continue;
        };
        for section in ["capture", "session", "continuation"] {
            let Some(base) = defaults.get(section).and_then(toml::Value::as_table) else {
                continue;
            };
            let mut resolved = base.clone();
            if let Some(toml::Value::Table(override_table)) = profile.get(section) {
                merge::merge_into(&mut resolved, override_table.clone());
            }
            profile.insert(section.to_owned(), toml::Value::Table(resolved));
        }
    }
}

/// Report keys the user wrote that the schema silently ignored.
///
/// A misspelled key that parses fine and does nothing is the worst kind of configuration
/// bug: everything looks correct and the setting simply has no effect.
fn unknown_key_issues(config: &Config, written: &Table) -> Vec<Issue> {
    let Ok(toml::Value::Table(round_tripped)) = toml::Value::try_from(config) else {
        // Serializing a value that just deserialized should not fail. If it somehow does,
        // skipping this check is better than refusing to start.
        return Vec::new();
    };

    // Free-form maps hold user-chosen keys — headers, env vars, provider form fields — so
    // their children round-trip verbatim and never need reporting. They are excluded by
    // path so that a typo in a *sibling* key is still caught.
    let mut free_form: Vec<String> = FREE_FORM_PREFIXES.iter().map(|p| (*p).to_owned()).collect();
    for name in config.transcribers.keys() {
        for field in ["headers", "form", "json", "query", "env"] {
            free_form.push(format!("transcribers.{name}.{field}."));
        }
    }
    for name in config.sinks.keys() {
        for field in ["headers", "env", "form"] {
            free_form.push(format!("sinks.{name}.{field}."));
        }
        free_form.push(format!("sinks.{name}.body.form."));
    }
    for name in config.presenters.keys() {
        free_form.push(format!("presenters.{name}.env."));
    }
    let free_form: Vec<&str> = free_form.iter().map(String::as_str).collect();

    merge::unknown_keys(written, &round_tripped, &free_form)
        .into_iter()
        .map(|path| {
            let issue = Issue::new(path.clone(), "unknown configuration key");
            match merge::suggest(&path, &round_tripped) {
                Some(close) => issue.with_hint(format!("did you mean `{close}`?")),
                None => issue,
            }
        })
        .collect()
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })
}
