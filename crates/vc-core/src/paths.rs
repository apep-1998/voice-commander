//! Where things live on disk.
//!
//! Plain XDG, with environment overrides honoured so a test — or a second instance — can be
//! pointed somewhere harmless without touching the user's real recordings.

use std::path::PathBuf;

const APP: &str = "voice-commander";

/// `$XDG_CONFIG_HOME/voice-commander`, falling back to `~/.config/voice-commander`.
pub fn config_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.config_dir().join(APP))
        .unwrap_or_else(|| PathBuf::from(".config").join(APP))
}

/// `$XDG_DATA_HOME/voice-commander`, where recordings and logs are kept.
pub fn data_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.data_dir().join(APP))
        .unwrap_or_else(|| PathBuf::from(".local/share").join(APP))
}

/// The control socket.
///
/// `$XDG_RUNTIME_DIR` is the right home for it: it is user-private, on tmpfs, and cleared at
/// logout, so a stale socket cannot outlive the session that created it. When it is unset —
/// which mostly means a container or a bare `ssh` session — `/tmp` with the uid in the name
/// is the conventional fallback.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join(format!("{APP}.sock")),
        None => PathBuf::from("/tmp").join(format!("{APP}-{}.sock", uid())),
    }
}

fn uid() -> String {
    std::env::var("UID").unwrap_or_else(|_| "0".to_owned())
}
