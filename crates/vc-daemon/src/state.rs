//! What the daemon knows, and the lock that guards it.

use std::path::PathBuf;
use std::time::Instant;

use vc_core::config::Config;
use vc_core::session::SessionId;
use vc_ipc::protocol::{ActivityState, DaemonStatus, DeviceStatus};

/// Everything mutable, behind one lock.
///
/// One lock rather than several because the interesting invariants span fields — "there is a
/// session if and only if the activity is not idle" cannot be maintained by locks that can
/// be taken separately. The critical sections are microseconds of bookkeeping; the audio
/// thread never touches this.
#[derive(Debug)]
pub struct State {
    pub config: Config,
    /// Warnings from the configuration currently in effect, kept so `status` can surface
    /// them long after the log line scrolled away.
    pub config_warnings: Vec<String>,
    pub activity: ActivityState,
    pub session: Option<SessionId>,
    pub profile: Option<String>,
    pub device: Option<DeviceStatus>,
    pub socket: PathBuf,
    /// Where the configuration is read from, so a reload does not have to be routed back out
    /// to whoever constructed the daemon.
    pub config_dir: PathBuf,
    started_at: Instant,
}

impl State {
    pub fn new(
        config: Config,
        config_warnings: Vec<String>,
        socket: PathBuf,
        config_dir: PathBuf,
    ) -> Self {
        Self {
            config,
            config_warnings,
            activity: ActivityState::Idle,
            session: None,
            profile: None,
            device: None,
            socket,
            config_dir,
            started_at: Instant::now(),
        }
    }

    pub fn status(&self) -> DaemonStatus {
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: vc_ipc::PROTOCOL_VERSION,
            uptime_secs: self.started_at.elapsed().as_secs(),
            activity: self.activity,
            session: self.session.clone(),
            profile: self.profile.clone(),
            device: self.device.clone(),
            // Sorted, because `status` doubles as "what can I bind?" and an arbitrary order
            // makes that list annoying to read.
            profiles: self.config.profiles.keys().cloned().collect(),
            config_warnings: self.config_warnings.len(),
            socket: self.socket.clone(),
        }
    }

    pub fn knows_profile(&self, name: &str) -> bool {
        self.config.profiles.contains_key(name)
    }
}
