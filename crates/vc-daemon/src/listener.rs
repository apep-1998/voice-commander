//! The control socket: binding it, and handling one client connection.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch, Mutex};
use tracing::{debug, info, warn};

use vc_ipc::protocol::{Command, ErrorCode, Request, Response, PROTOCOL_VERSION};

use crate::state::State;

/// Bind the control socket, clearing a stale one if that is what it is.
///
/// A socket file left behind by a crash is indistinguishable from a live one until something
/// tries to connect, so that is what this does: connect. Deleting on sight would let a second
/// daemon steal the socket from a perfectly healthy first one, and the two would then fight
/// over the microphone.
pub async fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    // Unix socket paths live in a fixed-size struct field, and the kernel's own error for
    // overflowing it — "path must be shorter than SUN_LEN" — sends people hunting for a
    // configuration key by that name. Say what is actually wrong instead.
    const SUN_PATH_MAX: usize = 107;
    let len = path.as_os_str().len();
    if len > SUN_PATH_MAX {
        bail!(
            "the socket path is {len} bytes, but a Unix socket path cannot exceed \
             {SUN_PATH_MAX}: {}\nset a shorter one with --socket or $VOICE_COMMANDER_SOCKET",
            path.display()
        );
    }

    if path.exists() {
        if vc_ipc::client::is_running(path, Duration::from_millis(250)) {
            bail!(
                "another voice-commander daemon is already listening on {}",
                path.display()
            );
        }
        info!(path = %path.display(), "removing a socket left behind by a previous run");
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding control socket {}", path.display()))?;

    // The socket carries commands that record audio. Nobody else on the machine gets to send
    // them, and `$XDG_RUNTIME_DIR` being 0700 is not something to rely on alone.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))?;

    Ok(listener)
}

/// What a handled command asks the daemon to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    Continue,
    Shutdown,
}

/// Serve one client until it disconnects, or until the daemon stops.
///
/// `stop` is not a nicety. A subscriber parks indefinitely waiting for the next event, and
/// the connection task holds a clone of the event sender — so without an explicit signal the
/// channel can never close and the task would outlive the daemon that spawned it.
pub async fn serve(
    stream: UnixStream,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<String>,
    mut stop: watch::Receiver<bool>,
) -> Directive {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    loop {
        let next = tokio::select! {
            next = lines.next_line() => next,
            _ = stop.changed() => return Directive::Continue,
        };
        let line = match next {
            Ok(Some(line)) => line,
            Ok(None) => return Directive::Continue,
            Err(error) => {
                debug!(%error, "client connection dropped");
                return Directive::Continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let (response, directive) = handle(&line, &state, &events).await;

        let mut payload = match serde_json::to_string(&response) {
            Ok(payload) => payload,
            Err(error) => {
                warn!(%error, "could not serialize a reply");
                return Directive::Continue;
            }
        };
        payload.push('\n');
        if write_half.write_all(payload.as_bytes()).await.is_err() {
            return Directive::Continue;
        }

        // A subscriber stops being a request/response client and becomes a firehose, so it
        // takes over the connection for the rest of its life.
        if let Some(filter) = subscription(&line) {
            stream_events(write_half, events, filter, stop).await;
            return Directive::Continue;
        }

        if directive != Directive::Continue {
            return directive;
        }
    }
}

/// The event-kind allow-list, if this line was a `subscribe`.
fn subscription(line: &str) -> Option<Vec<String>> {
    match serde_json::from_str::<Request>(line) {
        Ok(Request {
            command: Command::Subscribe { events },
            ..
        }) => Some(events),
        _ => None,
    }
}

async fn handle(
    line: &str,
    state: &Arc<Mutex<State>>,
    events: &broadcast::Sender<String>,
) -> (Response, Directive) {
    let request = match serde_json::from_str::<Request>(line) {
        Ok(request) => request,
        Err(error) => {
            return (
                Response::Error {
                    code: ErrorCode::BadRequest,
                    message: error.to_string(),
                },
                Directive::Continue,
            )
        }
    };

    if request.v != PROTOCOL_VERSION {
        // Upgrading the package while the old daemon is still running is entirely normal, so
        // say exactly what happened and what to do about it.
        return (
            Response::Error {
                code: ErrorCode::VersionMismatch,
                message: format!(
                    "client speaks protocol v{}, this daemon speaks v{PROTOCOL_VERSION}; \
                     restart the daemon after upgrading",
                    request.v
                ),
            },
            Directive::Continue,
        );
    }

    let mut guard = state.lock().await;
    match request.command {
        Command::Ping => (
            Response::Pong {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: PROTOCOL_VERSION,
            },
            Directive::Continue,
        ),
        Command::Status => (
            Response::Status(Box::new(guard.status())),
            Directive::Continue,
        ),
        Command::Subscribe { .. } => (Response::Subscribed, Directive::Continue),
        // Reloading happens here, while the lock is held, so the reply reports the warnings
        // from the configuration that is now in effect. Answering first and reloading
        // afterwards would hand the user the *previous* run's warnings — the one thing they
        // asked for by reloading.
        Command::Reload => (reload(&mut guard, events), Directive::Continue),
        Command::Shutdown => (Response::Accepted { session: None }, Directive::Shutdown),

        // The profile is resolved here rather than on the capture thread so that a typo in
        // a keybind comes back as an error the user can see, instead of a log line on a
        // thread nobody is watching.
        Command::Start { profile } => dispatch(&guard, &profile, |name| {
            crate::engine::Command::Start { profile: name }
        }),
        Command::Stop { profile } => dispatch(&guard, &profile, |name| {
            crate::engine::Command::Stop { profile: name }
        }),
        Command::Toggle { profile } => dispatch(&guard, &profile, |name| {
            crate::engine::Command::Toggle { profile: name }
        }),
        Command::Cancel => {
            let _ = guard.engine.send(crate::engine::Command::Cancel);
            (Response::Accepted { session: None }, Directive::Continue)
        }
    }
}

/// Validate the profile, then hand the command to the capture thread.
fn dispatch(
    state: &State,
    profile: &str,
    build: impl FnOnce(String) -> crate::engine::Command,
) -> (Response, Directive) {
    if !state.knows_profile(profile) {
        return (unknown_profile(profile, state), Directive::Continue);
    }
    match state.engine.send(build(profile.to_owned())) {
        Ok(()) => (
            // The session id is not known yet — the capture thread assigns it. A keybind
            // ignores this reply anyway; what matters is that it comes back immediately
            // rather than waiting for a device to open.
            Response::Accepted { session: None },
            Directive::Continue,
        ),
        Err(error) => (
            Response::Error {
                code: ErrorCode::Internal,
                message: format!("the capture thread is not running: {error}"),
            },
            Directive::Continue,
        ),
    }
}

/// Re-read the configuration, keeping the current one if the new one is unusable.
///
/// Refusing a bad reload rather than exiting matters: the daemon holds the microphone, and a
/// typo saved in an editor must not silently end the user's ability to record.
fn reload(state: &mut State, events: &broadcast::Sender<String>) -> Response {
    match vc_core::config::Config::load_from_dir(&state.config_dir) {
        Ok(loaded) => {
            let warnings: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
            for warning in &warnings {
                warn!("{warning}");
            }
            let _ = state
                .engine
                .send(crate::engine::Command::Reconfigure(Box::new(
                    loaded.config.clone(),
                )));
            state.config = loaded.config;
            state.config_warnings.clone_from(&warnings);
            info!(warnings = warnings.len(), "configuration reloaded");
            publish(
                events,
                vc_core::event::Event::ConfigReloaded {
                    warnings: warnings.len(),
                },
            );
            Response::Reloaded { warnings }
        }
        Err(error) => {
            warn!(%error, "configuration is invalid; keeping the one already in effect");
            publish(
                events,
                vc_core::event::Event::Error {
                    stage: vc_core::event::Stage::Config,
                    message: error.to_string(),
                },
            );
            Response::Error {
                code: ErrorCode::ConfigInvalid,
                message: format!("{error}\n(the previous configuration is still in effect)"),
            }
        }
    }
}

fn publish(events: &broadcast::Sender<String>, event: vc_core::event::Event) {
    if let Ok(line) =
        vc_core::Envelope::for_daemon(time::OffsetDateTime::now_utc(), event).to_ndjson()
    {
        let _ = events.send(line);
    }
}

fn unknown_profile(profile: &str, state: &State) -> Response {
    let mut known: Vec<&str> = state.config.profiles.keys().map(String::as_str).collect();
    known.sort_unstable();
    Response::Error {
        code: ErrorCode::UnknownProfile,
        message: format!(
            "no profile named {profile:?}; configured profiles are: {}",
            known.join(", ")
        ),
    }
}

/// Forward events to a subscriber until it goes away.
async fn stream_events(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    events: broadcast::Sender<String>,
    filter: Vec<String>,
    mut stop: watch::Receiver<bool>,
) {
    let mut rx = events.subscribe();
    loop {
        let received = tokio::select! {
            received = rx.recv() => received,
            _ = stop.changed() => return,
        };
        match received {
            Ok(line) => {
                if !filter.is_empty() && !matches_filter(&line, &filter) {
                    continue;
                }
                if write_half
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // A subscriber that cannot keep up loses events rather than stalling the daemon.
            // Dropping frames of a level meter is fine; blocking the recording pipeline on a
            // slow status bar is not.
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                debug!(skipped, "a subscriber fell behind");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn matches_filter(line: &str, filter: &[String]) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("event")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|kind| filter.iter().any(|wanted| wanted == &kind))
}

/// Remove the socket on the way out, so the next start does not have to reason about it.
pub fn cleanup(path: &PathBuf) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(%error, path = %path.display(), "could not remove the control socket");
        }
    }
}
