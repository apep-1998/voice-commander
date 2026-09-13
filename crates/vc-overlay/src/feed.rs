//! Where events come from, so the draw loop does not care which.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::Result;
use vc_core::event::Event;

use crate::demo::Demo;

pub(crate) enum Feed {
    /// A background thread reading the control socket.
    Daemon(Receiver<vc_core::Envelope>),
    /// A scripted session, looping.
    Demo(Box<Demo>),
}

impl std::fmt::Debug for Feed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Daemon(_) => f.write_str("Feed::Daemon"),
            Self::Demo(_) => f.write_str("Feed::Demo"),
        }
    }
}

impl Feed {
    /// Everything that has arrived since the last frame.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<Event> {
        match self {
            Self::Demo(demo) => {
                let mut events = Vec::new();
                if let Some(level) = demo.level(now) {
                    events.push(level);
                }
                events.extend(demo.poll(now));
                events
            }
            Self::Daemon(rx) => {
                // Disconnection needs no handling here: the reader thread reconnects, and
                // until it does there is simply nothing to drain.
                rx.try_iter().map(|envelope| envelope.event).collect()
            }
        }
    }
}

/// Subscribe on a background thread, reconnecting if the daemon restarts.
///
/// An overlay that died when the daemon was restarted would have to be restarted too, which
/// for something bound into session startup is a poor arrangement.
pub(crate) fn subscribe(socket: PathBuf) -> Result<Receiver<vc_core::Envelope>> {
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("vc-overlay-events".to_owned())
        .spawn(move || loop {
            match vc_ipc::Client::connect(&socket, Duration::from_secs(2))
                .and_then(|client| client.subscribe(Vec::new()))
            {
                Ok(stream) => {
                    for line in stream {
                        let Ok(line) = line else { break };
                        let Ok(envelope) = vc_core::Envelope::from_ndjson(&line) else {
                            continue;
                        };
                        if tx.send(envelope).is_err() {
                            return; // The overlay is gone.
                        }
                    }
                }
                Err(error) => {
                    eprintln!("waiting for the daemon: {error}");
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        })?;

    Ok(rx)
}
