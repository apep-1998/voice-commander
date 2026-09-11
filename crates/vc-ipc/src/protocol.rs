//! The request and response types, shared by both sides so they cannot drift apart.
//!
//! One JSON object per line, in both directions. Line-delimited rather than
//! length-prefixed because the whole protocol should be usable from a shell:
//!
//! ```sh
//! echo '{"v":1,"cmd":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/voice-commander.sock
//! ```
//!
//! Being able to drive and inspect the daemon without its own client is worth more than the
//! handful of bytes a binary framing would save.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use vc_core::session::SessionId;

/// Protocol version.
///
/// Separate from the event schema version: this one changes when the *command* surface
/// changes, which is a different concern from what the event stream emits. Carried on every
/// request so that a client left over from a previous install gets told plainly rather than
/// failing in some obscure way — upgrading the package while the old daemon is still running
/// is entirely normal.
pub const PROTOCOL_VERSION: u32 = 1;

/// A message from a client to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Liveness check. Also how the client tells a stale socket from a running daemon.
    Ping,
    /// Begin recording, or extend the session already in its cooldown window.
    Start { profile: String },
    /// Stop recording and open the cooldown window.
    ///
    /// Idempotent: stopping something already stopped is success, not an error. A compositor
    /// can deliver a release without a matching press — and a keybind that errors when the
    /// user did nothing wrong is worse than one that quietly does nothing.
    Stop { profile: String },
    /// Flip between recording and not, for `trigger = "toggle"` profiles.
    Toggle { profile: String },
    /// Discard whatever is in flight. Nothing downstream runs.
    Cancel,
    /// A snapshot of what the daemon is doing.
    Status,
    /// Turn this connection into a one-way event stream.
    ///
    /// `events` is an allow-list of event kinds; empty means everything. A bar module that
    /// only cares about start and stop should not be woken twenty times a second by level
    /// updates.
    Subscribe {
        #[serde(default)]
        events: Vec<String>,
    },
    /// Re-read the configuration from disk.
    Reload,
    /// Shut the daemon down cleanly, finishing any session in flight.
    Shutdown,
}

/// A message from the daemon to a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Pong {
        version: String,
        protocol: u32,
    },
    /// The command was acted on. `session` is present when one was started or extended.
    Accepted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<SessionId>,
    },
    Status(Box<DaemonStatus>),
    Reloaded {
        warnings: Vec<String>,
    },
    /// The stream is open; every following line is an event envelope.
    Subscribed,
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Why a command failed.
///
/// A closed set so that a caller can branch on it — a script wrapping the client needs to
/// tell "you typed a profile that does not exist" from "the microphone is gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was not valid JSON, or not a known command.
    BadRequest,
    /// The client speaks a different protocol version than the daemon.
    VersionMismatch,
    /// No `[profiles.*]` entry by that name.
    UnknownProfile,
    /// The command does not apply in the daemon's current state.
    WrongState,
    /// The audio device could not be opened.
    DeviceUnavailable,
    /// Configuration on disk is not valid, so the reload was refused and the previous
    /// configuration is still in effect.
    ConfigInvalid,
    /// Something unexpected. The message carries the detail.
    Internal,
    /// Recognised, but this build does not implement it yet.
    NotImplemented,
}

impl ErrorCode {
    /// Process exit code for a client that hit this error.
    ///
    /// Distinct codes so a keybind wrapper script can react without parsing text.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::BadRequest | Self::VersionMismatch => 2,
            Self::UnknownProfile => 3,
            Self::WrongState => 4,
            Self::DeviceUnavailable => 5,
            Self::ConfigInvalid => 6,
            Self::NotImplemented => 7,
            Self::Internal => 1,
        }
    }
}

/// What the daemon is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActivityState {
    /// Nothing in flight.
    Idle,
    /// Capturing.
    Recording { segment: u32, elapsed_ms: u64 },
    /// Stopped, but inside the continuation window — pressing again resumes this session.
    Cooling { remaining_ms: u32 },
    /// Transcribing or running callbacks.
    Processing,
}

/// The open capture device, if there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// How much lookback the pre-roll buffer currently holds, which right after the device
    /// opens is less than the configured amount.
    pub pre_roll_available_ms: u32,
}

/// A snapshot of the daemon, for `voice-commander status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub protocol: u32,
    pub uptime_secs: u64,
    pub activity: ActivityState,
    /// The session in flight, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceStatus>,
    /// Profile names from the loaded configuration, so `status` doubles as "what can I
    /// bind?".
    pub profiles: Vec<String>,
    /// Warnings raised by the configuration currently in effect. Non-zero is worth showing.
    pub config_warnings: usize,
    pub socket: PathBuf,
}
