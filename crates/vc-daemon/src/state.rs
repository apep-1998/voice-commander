//! What the daemon knows, and the lock that guards it.

use std::path::PathBuf;
use std::time::Instant;

use std::sync::mpsc::Sender;

use tokio::sync::watch;
use vc_core::config::Config;
use vc_ipc::protocol::DaemonStatus;

use crate::engine;

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
    /// Commands to the capture thread. It owns the microphone; nothing else touches it.
    pub engine: Sender<engine::Command>,
    /// The capture thread's own view of what it is doing, published as it changes.
    pub engine_status: watch::Receiver<engine::Status>,
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
        engine: Sender<engine::Command>,
        engine_status: watch::Receiver<engine::Status>,
    ) -> Self {
        Self {
            config,
            config_warnings,
            engine,
            engine_status,
            socket,
            config_dir,
            started_at: Instant::now(),
        }
    }

    pub fn status(&self) -> DaemonStatus {
        let engine = self.engine_status.borrow().clone();
        DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: vc_ipc::PROTOCOL_VERSION,
            uptime_secs: self.started_at.elapsed().as_secs(),
            activity: engine.activity,
            session: engine.session,
            profile: engine.profile,
            device: engine.device,
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
