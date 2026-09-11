//! The voice-commander daemon.
//!
//! Exposed as a library as well as a binary so that integration tests can start a real
//! daemon on a temporary socket, in-process, and drive it through the same client a keybind
//! uses. A test that talks to a mock instead of the real accept loop proves nothing about
//! the accept loop.

pub mod engine;
pub mod listener;
pub mod recorder;
pub mod state;
pub mod storage;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{error, info, warn};

use vc_core::config::Config;
use vc_core::event::Event;
use vc_core::Envelope;

use listener::Directive;
use state::State;
use storage::Storage;

/// How many events a subscriber may fall behind before it starts losing them.
///
/// Levels arrive twenty times a second, so this is several seconds of slack — enough for a
/// status bar to be descheduled without noticing, and far short of letting a wedged consumer
/// pin memory.
const EVENT_BUFFER: usize = 256;

/// A running daemon.
pub struct Daemon {
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<String>,
    socket: PathBuf,
    /// Joined on shutdown so the capture thread gets to finish writing whatever it was
    /// recording, rather than having the process exit out from under it.
    capture: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Load configuration and prepare to serve, without binding anything yet.
    pub fn new(config_dir: &Path, socket: PathBuf) -> anyhow::Result<Self> {
        Self::with_storage(config_dir, socket, None)
    }

    /// As [`Daemon::new`], but with recordings written somewhere other than the XDG data
    /// directory. Tests use this so a run never touches the user's real recordings.
    pub fn with_storage(
        config_dir: &Path,
        socket: PathBuf,
        data_dir: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let loaded = Config::load_from_dir(config_dir).context("loading configuration")?;

        let warnings: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
        for warning in &warnings {
            warn!("{warning}");
        }

        let (events, _) = broadcast::channel(EVENT_BUFFER);

        let root = data_dir
            .or_else(|| loaded.config.storage.dir.clone())
            .unwrap_or_else(vc_core::paths::data_dir);
        let (engine_tx, engine_status, capture) =
            engine::spawn(loaded.config.clone(), Storage::new(root), events.clone())?;

        Ok(Self {
            state: Arc::new(Mutex::new(State::new(
                loaded.config,
                warnings,
                socket.clone(),
                config_dir.to_owned(),
                engine_tx,
                engine_status,
            ))),
            events,
            socket,
            capture: Some(capture),
        })
    }

    /// Publish an event to every subscriber.
    ///
    /// Failure here means nobody is listening, which is the normal case and not worth
    /// mentioning.
    pub fn publish(&self, envelope: &Envelope) {
        if let Ok(line) = envelope.to_ndjson() {
            let _ = self.events.send(line);
        }
    }

    /// Serve until told to shut down, or until `shutdown` resolves.
    ///
    /// Taking an explicit shutdown future rather than only listening for signals is what lets
    /// a test stop the daemon deterministically instead of by killing a process.
    pub async fn run(
        mut self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let listener = listener::bind(&self.socket).await?;
        info!(socket = %self.socket.display(), "listening");

        self.publish(&Envelope::for_daemon(
            now(),
            Event::DaemonReady {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                socket: self.socket.clone(),
            },
        ));

        // Every connection task watches this. Dropping the listener is not enough to end
        // them: a subscriber is parked on the event channel, not on the socket.
        let (stop_tx, stop_rx) = watch::channel(false);
        let result = self.accept_loop(listener, shutdown, stop_rx).await;
        let _ = stop_tx.send(true);

        // Let the capture thread finish first: it may be part-way through writing a
        // recording, and exiting out from under it would lose what the user just said.
        {
            let guard = self.state.lock().await;
            let _ = guard.engine.send(engine::Command::Shutdown);
        }
        if let Some(capture) = self.capture.take() {
            let _ = tokio::task::spawn_blocking(move || capture.join()).await;
        }

        listener::cleanup(&self.socket);
        result
    }

    async fn accept_loop(
        &self,
        listener: tokio::net::UnixListener,
        shutdown: impl std::future::Future<Output = ()>,
        stop_rx: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        // Set when a client sends `shutdown`, which has to unwind the accept loop from inside
        // a connection task.
        let (quit_tx, mut quit_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => {
                    info!("shutting down");
                    return Ok(());
                }
                _ = quit_rx.recv() => {
                    info!("shutdown requested over the control socket");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            // One task per client: a `subscribe` holds its connection open
                            // indefinitely, and must not stop anyone else from being served.
                            let state = Arc::clone(&self.state);
                            let events = self.events.clone();
                            let quit = quit_tx.clone();
                            let stop_rx = stop_rx.clone();
                            tokio::spawn(async move {
                                match listener::serve(stream, state, events, stop_rx).await {
                                    Directive::Continue => {}
                                    Directive::Shutdown => {
                                        let _ = quit.send(()).await;
                                    }
                                }
                            });
                        }
                        Err(error) => {
                            error!(%error, "could not accept a connection");
                        }
                    }
                }
            }
        }
    }
}

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}
