//! Configuration diagnostics.
//!
//! Loading reports *every* problem it can find rather than stopping at the first. Editing a
//! config file, running the daemon, fixing one typo and repeating is a miserable loop; one
//! pass that lists everything wrong is worth the small amount of extra machinery.

use std::fmt;
use std::path::PathBuf;

/// One thing wrong with, or worth mentioning about, a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Dotted path to the offending key, e.g. `profiles.dictate.session.cooldown_ms`.
    pub path: String,
    pub message: String,
    /// What to do about it, when that is not obvious from the message.
    pub hint: Option<String>,
}

impl Issue {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n    hint: {hint}")?;
        }
        Ok(())
    }
}

/// Why a configuration could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// A file could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A layer was not valid TOML.
    Syntax {
        source_name: String,
        message: String,
    },
    /// The TOML parsed, but does not describe a usable configuration.
    Invalid { issues: Vec<Issue> },
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "reading {}: {source}", path.display()),
            Self::Syntax {
                source_name,
                message,
            } => write!(f, "{source_name} is not valid TOML: {message}"),
            Self::Invalid { issues } => {
                let plural = if issues.len() == 1 { "" } else { "s" };
                write!(f, "{} configuration problem{plural}:", issues.len())?;
                for issue in issues {
                    write!(f, "\n  - {issue}")?;
                }
                Ok(())
            }
        }
    }
}
