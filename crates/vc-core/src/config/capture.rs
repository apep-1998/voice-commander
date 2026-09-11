//! How the microphone behaves before, during and after a recording.

use serde::{Deserialize, Serialize};

/// What the daemon does with the audio device between recordings.
///
/// This is the latency-versus-power tradeoff, and it is deliberately the user's to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Keep the stream open and retain a rolling window of recent audio, so a recording can
    /// begin *before* the moment the key was pressed. Costs an open device and a fixed
    /// amount of memory; buys back the syllable that every other approach clips off.
    Preroll,
    /// Keep the stream open but discard everything until a recording starts. No lookback,
    /// but no device-open latency either.
    Warm,
    /// Open the device when a recording starts and close it when the recording ends. Lowest
    /// idle power draw, at the cost of roughly 100-200ms of stream setup during which speech
    /// is lost.
    OnDemand,
}

impl CaptureMode {
    /// Whether this mode holds the device open while idle.
    pub fn keeps_device_open(self) -> bool {
        matches!(self, Self::Preroll | Self::Warm)
    }
}

/// Which input device to capture from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceSelector {
    /// Follow whatever the system default input is, including when it changes.
    #[default]
    Default,
    /// Use the first device whose name contains this (case-insensitive) substring.
    Match(String),
}

impl DeviceSelector {
    /// Whether `name` satisfies this selector. `Default` matches nothing on its own; the
    /// caller resolves it through the host's default-device lookup instead.
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::Default => false,
            Self::Match(needle) => name.to_lowercase().contains(&needle.to_lowercase()),
        }
    }
}

impl<'de> Deserialize<'de> for DeviceSelector {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(if raw == "default" {
            Self::Default
        } else {
            Self::Match(raw)
        })
    }
}

impl Serialize for DeviceSelector {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => s.serialize_str("default"),
            Self::Match(name) => s.serialize_str(name),
        }
    }
}

/// Fully resolved capture settings for one profile.
///
/// Every field is concrete: the loader merges `[defaults.capture]` with any per-profile
/// override before this is deserialized, so nothing downstream deals in `Option`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub mode: CaptureMode,
    /// How much audio from before the keypress to include, in `Preroll` mode.
    pub pre_roll_ms: u32,
    /// Close the device after this many seconds with no recording. `0` never closes it.
    /// Only meaningful in modes that hold the device open.
    pub idle_release_secs: u32,
    pub device: DeviceSelector,
}
