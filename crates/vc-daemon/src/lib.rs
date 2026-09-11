//! The voice-commander daemon.
//!
//! Exposed as a library as well as a binary so that integration tests can start a real
//! daemon on a temporary socket, in-process, and drive it through the same client a keybind
//! uses. A test that talks to a mock instead of the real accept loop proves nothing about
//! the accept loop.

pub mod engine;
pub mod feedback;
pub mod inflight;
pub mod listener;
pub mod pipeline;
pub mod presenters;
pub mod recorder;
pub mod retention;
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

/// How long to let running pipelines finish when shutting down.
///
/// Long enough for a transcription and a webhook to complete, short enough that a user who
/// asked the daemon to stop is not left waiting on someone else's slow server.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

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
    /// Where recordings and the event log live.
    data_dir: PathBuf,
    /// Joined on shutdown so the capture thread gets to finish writing whatever it was
    /// recording, rather than having the process exit out from under it.
    capture: Option<std::thread::JoinHandle<()>>,
    /// Pipelines still running. Waited on for the same reason.
    in_flight: inflight::InFlight,
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
        // The capture thread needs a handle to spawn pipeline tasks onto, and `Daemon::new`
        // is always called from inside the runtime.
        let runtime = tokio::runtime::Handle::try_current()
            .context("the daemon must be constructed inside a tokio runtime")?;
        let root_for_daemon = root.clone();
        let in_flight = inflight::InFlight::new();
        let (engine_tx, engine_status, capture) = engine::spawn(
            loaded.config.clone(),
            Storage::new(root),
            events.clone(),
            runtime,
            in_flight.clone(),
        )?;

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
            data_dir: root_for_daemon,
            capture: Some(capture),
            in_flight,
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

        // Presenters subscribe to the same bus everything else reads, and run as their own
        // tasks so a slow one falls behind and loses events rather than stalling a recording.
        let presenters = {
            let guard = self.state.lock().await;
            presenters::start(&guard.config, &self.data_dir, &self.events, stop_rx.clone())
        };
        let result = self.accept_loop(listener, shutdown, stop_rx).await;

        // Deliberately not signalling `stop` yet. Everything below still produces events —
        // the capture thread finalises its last recording, the pipelines finish and report —
        // and telling the presenters to stop first would throw exactly those away.
        //
        // Let the capture thread finish first: it may be part-way through writing a
        // recording, and exiting out from under it would lose what the user just said.
        {
            let guard = self.state.lock().await;
            let _ = guard.engine.send(engine::Command::Shutdown);
        }
        if let Some(capture) = self.capture.take() {
            let _ = tokio::task::spawn_blocking(move || capture.join()).await;
        }

        // Then let the pipelines finish. The capture thread hands its last session off on
        // the way out, so this has to come second — and without it, a transcript and a set
        // of callback results that already completed would never reach session.json.
        self.in_flight.drain(SHUTDOWN_GRACE).await;

        // Then let the presenters flush. The event log in particular has the last events of
        // the session sitting in a buffer.
        let _ = stop_tx.send(true);
        for task in presenters {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
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
