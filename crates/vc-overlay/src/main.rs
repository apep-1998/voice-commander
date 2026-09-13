//! `voice-commander-overlay` — a heads-up display for the daemon.
//!
//! It subscribes to the event stream over the control socket and draws what it finds. That
//! is deliberately all the access it has: the same stream a shell script reads, with no
//! privileged channel of its own. If this can be built against the published contract, so
//! can anything else.
//!
//! Run `--demo` to watch a scripted session without a microphone, a key, or a keybind.

mod demo;
mod draw;
mod shell;
mod state;
mod theme;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;

use draw::Renderer;
use shell::{Position, Shell};
use state::Overlay;
use theme::{Metrics, Theme};

#[derive(Debug, Parser)]
#[command(
    name = "voice-commander-overlay",
    version,
    about = "A heads-up display for voice-commander",
    long_about = "Draws what the daemon is doing: a live level ring while you speak, and a \
                  per-callback progress panel while the pipeline runs.\n\nIt reads the same \
                  event stream `voice-commander events --follow` does, and has no other \
                  access to the daemon."
)]
struct Args {
    /// Play a scripted session on a loop, without needing the daemon.
    #[arg(long)]
    demo: bool,

    /// Where on screen to sit.
    #[arg(long, value_enum, default_value = "bottom")]
    position: PositionArg,

    /// Distance from that screen edge, in pixels.
    #[arg(long, default_value_t = 72)]
    margin: i32,

    /// Diameter of the ring, in pixels.
    #[arg(long, default_value_t = 240)]
    size: u32,

    /// Control socket. Defaults to $VOICE_COMMANDER_SOCKET, then the XDG runtime directory.
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PositionArg {
    Bottom,
    Top,
    Centre,
    BottomRight,
}

impl From<PositionArg> for Position {
    fn from(value: PositionArg) -> Self {
        match value {
            PositionArg::Bottom => Self::BottomCentre,
            PositionArg::Top => Self::TopCentre,
            PositionArg::Centre => Self::Centre,
            PositionArg::BottomRight => Self::BottomRight,
        }
    }
}

/// Where events come from, so the draw loop does not care which.
enum Feed {
    /// A background thread reading the socket.
    Daemon(Receiver<vc_core::Envelope>),
    Demo(Box<demo::Demo>),
}

fn main() -> Result<()> {
    let args = Args::parse();

    let metrics = Metrics {
        size: args.size,
        ..Metrics::default()
    };
    let overlay = Overlay::new(metrics.bars);
    let renderer = Renderer::new(Theme::default(), metrics);

    let mut feed = if args.demo {
        eprintln!("demo: playing a scripted session on a loop — Ctrl-C to stop");
        Feed::Demo(Box::new(demo::Demo::new(Instant::now())))
    } else {
        let socket = vc_ipc::client::resolve_socket_path(args.socket);
        Feed::Daemon(subscribe(socket)?)
    };

    let (mut shell, connection, mut queue) =
        Shell::new(overlay, renderer, args.position.into(), args.margin)?;

    // Drives the event loop at 60 Hz. The ring animates continuously while visible, and
    // idles quietly the rest of the time.
    loop {
        queue.blocking_dispatch(&mut shell)?;
        if shell.exit {
            break;
        }

        let now = Instant::now();
        let mut changed = shell.overlay.tick(now);

        match &mut feed {
            Feed::Demo(demo) => {
                if let Some(level) = demo.level(now) {
                    shell.overlay.apply(&level, now);
                    changed = true;
                }
                for event in demo.poll(now) {
                    shell.overlay.apply(&event, now);
                    changed = true;
                }
            }
            Feed::Daemon(rx) => loop {
                match rx.try_recv() {
                    Ok(envelope) => {
                        shell.overlay.apply(&envelope.event, now);
                        changed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        eprintln!("the daemon closed the connection");
                        return Ok(());
                    }
                }
            },
        }

        if changed || shell.overlay.visible() {
            shell.render(&queue.handle(), now);
        }

        connection.flush()?;
        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

/// Subscribe on a background thread, reconnecting if the daemon restarts.
///
/// An overlay that died when the daemon was restarted would have to be restarted too, which
/// for something bound into a session startup is a poor arrangement.
fn subscribe(socket: PathBuf) -> Result<Receiver<vc_core::Envelope>> {
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
                            return;
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
