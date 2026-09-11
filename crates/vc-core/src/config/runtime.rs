//! Configuration for the daemon itself: audio format, storage, feedback and logging.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Daemon-wide settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Override the control socket path. Defaults to `$XDG_RUNTIME_DIR/voice-commander.sock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<PathBuf>,
    /// Let the client start the daemon if it is not already running. Convenient, but it
    /// means the first keypress after a reboot pays the startup cost the daemon exists to
    /// avoid — running it as a user service is better.
    #[serde(default)]
    pub autostart: bool,
    /// `tracing` filter, e.g. `info` or `vc_daemon=debug,vc_audio=trace`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_log_level() -> String {
    "info".to_owned()
}

/// The format recordings are stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    /// Uncompressed. Costs disk, but encoding is free and every speech-to-text API accepts it.
    #[default]
    Wav,
    /// Lossless and roughly half the size.
    Flac,
    /// Lossy and far smaller. Good for archives, but check your provider accepts it.
    Opus,
}

impl AudioFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Opus => "opus",
        }
    }
}

/// How captured audio is normalised before being stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Target rate for the stored file. 16 kHz is what speech models want, and it is roughly
    /// a sixth of the size of 48 kHz stereo with no accuracy cost for speech.
    pub sample_rate: u32,
    pub channels: u16,
    pub format: AudioFormat,
}

/// Thresholds that turn raw levels into the warnings a user can act on.
///
/// These exist so the feedback can say "your microphone is muted" rather than showing a flat
/// line and leaving the user to work it out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelsConfig {
    /// Below this peak, treat the input as silent.
    pub silence_dbfs: f32,
    /// Below this RMS while speech is expected, warn that the microphone is too far away or
    /// its gain is too low.
    pub too_quiet_dbfs: f32,
    /// Above this RMS counts as speech rather than room tone.
    pub speech_dbfs: f32,
    /// How long silence must persist during a recording before it is worth mentioning.
    pub silence_warn_after_ms: u32,
}

/// Where recordings and logs live, and when they are cleaned up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Override the data directory. Defaults to `$XDG_DATA_HOME/voice-commander`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// Delete recordings older than this. `0` keeps them forever.
    pub max_age_days: u32,
    /// Delete oldest recordings once the directory exceeds this. `0` means no limit.
    pub max_total_bytes: u64,
    /// Keep the audio file once a transcript exists. Turning this off makes the tool
    /// leave far less behind.
    pub keep_audio_after_transcribe: bool,
}

/// What the user is shown while recording and while callbacks run.
///
/// No graphical presenter ships yet. What ships is the event stream that one would consume,
/// which is deliberate: the same contract serves a future overlay, a bar module, or a shell
/// script, and none of them is privileged over the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackConfig {
    /// Master switch. Off means no presenter is started and no feedback events are emitted.
    pub enabled: bool,
    /// Emit the recording-and-levels events that drive a "listening" indicator.
    pub listening: bool,
    /// Emit the per-callback progress events that drive a "processing" indicator.
    pub processing: bool,
    /// How long a processing indicator should linger after the last callback finishes, so
    /// the result is readable rather than a flash.
    pub processing_linger_ms: u32,
    /// Minimum spacing between `level` events. Twenty a second is smooth to look at and
    /// cheap to produce; there is no reason to emit one per audio buffer.
    pub level_interval_ms: u32,
    /// Named entries from `[presenters.*]` to run.
    pub presenters: Vec<String>,
}

/// One named entry under `[presenters.*]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenterConfig {
    #[serde(flatten)]
    pub kind: PresenterKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenterKind {
    /// Publish events to anything connected to the control socket. This is what
    /// `voice-commander events --follow` reads, and what a future overlay would use.
    Socket(SocketPresenter),
    /// Run a command for each event, with the event JSON on standard input. The shell-script
    /// path to a custom indicator.
    Command(CommandPresenter),
    /// Raise desktop notifications at the start and end of a session. Coarse, but it works
    /// on any desktop with no extra software.
    Notify(NotifyPresenter),
}

impl PresenterKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Socket(_) => "socket",
            Self::Command(_) => "command",
            Self::Notify(_) => "notify",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SocketPresenter {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPresenter {
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Only forward these event kinds. Empty forwards everything.
    #[serde(default)]
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NotifyPresenter {
    /// Also notify on every callback result, not just the session outcome.
    #[serde(default)]
    pub per_sink: bool,
}
