//! A profile is what a keybind selects: one capture behaviour, at most one transcriber, and
//! any number of callbacks.

use serde::{Deserialize, Serialize};

use super::capture::CaptureConfig;
use super::session::{ContinuationConfig, SessionConfig, TriggerMode};
use super::sink::SinkMode;

/// One named entry under `[profiles.*]`, with every inherited value already resolved.
///
/// The loader merges `[defaults.*]` with any per-profile override *before* this is
/// deserialized, which is why `capture`, `session` and `continuation` are concrete rather
/// than a nest of `Option`s. Nothing downstream has to think about inheritance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub trigger: TriggerMode,
    /// Name of an entry in `[transcribers.*]`, or `false` for no transcription at all.
    ///
    /// Speech-to-text being optional is a first-class case, not a degraded one: a profile
    /// that hands a `.wav` straight to a script is a perfectly good profile.
    #[serde(default, with = "transcriber_ref")]
    pub transcriber: Option<String>,
    /// Names of entries in `[sinks.*]`. May be empty — the recording is stored either way,
    /// so an "archive only" profile is legitimate.
    #[serde(default)]
    pub sinks: Vec<String>,
    #[serde(default)]
    pub sink_mode: SinkMode,
    pub capture: CaptureConfig,
    pub session: SessionConfig,
    pub continuation: ContinuationConfig,
}

/// Accepts either a transcriber name or `false`.
///
/// Serializing `None` back as `false` rather than omitting the key keeps the round-trip
/// faithful, which the loader's unknown-key check relies on.
mod transcriber_ref {
    use serde::de::{Error, Unexpected};
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Name(String),
        Disabled(bool),
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        match Option::<Raw>::deserialize(d)? {
            None | Some(Raw::Disabled(false)) => Ok(None),
            Some(Raw::Disabled(true)) => Err(D::Error::invalid_value(
                Unexpected::Bool(true),
                &"a transcriber name, or `false` to disable transcription",
            )),
            Some(Raw::Name(name)) => Ok(Some(name)),
        }
    }

    pub(super) fn serialize<S: Serializer>(
        value: &Option<String>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(name) => s.serialize_str(name),
            None => s.serialize_bool(false),
        }
    }
}
