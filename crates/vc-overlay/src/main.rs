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
mod feed;
mod shell;
mod state;
mod theme;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;

use draw::Renderer;
use feed::Feed;
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

    /// Distance from that screen edge, in pixels. Ignored when centred.
    #[arg(long, default_value_t = 72)]
    margin: i32,

    /// Diameter of the ring, in pixels.
    #[arg(long, default_value_t = 240)]
    size: u32,

    /// Frames per second while something is moving.
    ///
    /// The ring is drawn in software, so this is the main thing that decides what it costs.
    /// 30 is smooth enough for a level meter and roughly halves the work.
    #[arg(long, default_value_t = 60)]
    fps: u32,

    /// Control socket. Defaults to $VOICE_COMMANDER_SOCKET, then the XDG runtime directory.
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PositionArg {
    Bottom,
    Top,
    Center,
    Centre,
    BottomRight,
    BottomLeft,
}

impl From<PositionArg> for Position {
    fn from(value: PositionArg) -> Self {
        match value {
            PositionArg::Bottom => Self::BottomCentre,
            PositionArg::Top => Self::TopCentre,
            // Both spellings, because half the world writes one and half the other.
            PositionArg::Center | PositionArg::Centre => Self::Centre,
            PositionArg::BottomRight => Self::BottomRight,
            PositionArg::BottomLeft => Self::BottomLeft,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let metrics = Metrics {
        size: args.size,
        ..Metrics::default()
    };
    let overlay = Overlay::new(metrics.bars);
    let renderer = Renderer::new(Theme::default(), metrics);

    let feed = if args.demo {
        eprintln!("demo: playing a scripted session on a loop — Ctrl-C to stop");
        Feed::Demo(Box::new(demo::Demo::new(std::time::Instant::now())))
    } else {
        let socket = vc_ipc::client::resolve_socket_path(args.socket);
        Feed::Daemon(feed::subscribe(socket)?)
    };

    let animating = args.demo;
    let (mut shell, connection, queue) = Shell::new(
        overlay,
        renderer,
        feed,
        args.position.into(),
        args.margin,
        args.fps,
    )?;
    shell.animating = animating;

    // A timer decides when to draw, not the compositor's frame callbacks: a callback is
    // answered immediately for a surface that is never presented, so using it as a clock
    // spins a core. calloop also lets the loop sleep properly between frames.
    let mut event_loop: EventLoop<Shell> = EventLoop::try_new()?;
    WaylandSource::new(connection, queue).insert(event_loop.handle())?;

    event_loop
        .handle()
        .insert_source(Timer::immediate(), |deadline, _, shell| {
            shell.timer_ticks += 1;
            shell.render(std::time::Instant::now());
            // Scheduled from the deadline rather than from now, so drawing time does not
            // stretch the period — `ToDuration` measures from when this returns, which
            // turned a 16ms frame into 21ms and 60fps into 47.
            let next = deadline + shell.next_frame();
            let now = std::time::Instant::now();
            TimeoutAction::ToInstant(if next > now { next } else { now })
        })
        .map_err(|error| anyhow::anyhow!("cannot start the frame timer: {error}"))?;

    while !shell.exit {
        event_loop.dispatch(Some(std::time::Duration::from_millis(200)), &mut shell)?;
    }

    Ok(())
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    /// Not a correctness test — it prints where the frame budget goes.
    /// `cargo test -p vc-overlay --release -- --ignored --nocapture bench`
    #[test]
    #[ignore = "timing, not correctness"]
    fn how_long_does_a_frame_take() {
        use vc_core::event::{Event, PlannedSink};

        let metrics = Metrics::default();
        let mut overlay = Overlay::new(metrics.bars);
        let mut renderer = Renderer::new(Theme::default(), metrics);
        let now = Instant::now();

        overlay.apply(
            &Event::RecordingStarted {
                segment: 0,
                pre_roll_ms: 500,
                device: "m".to_owned(),
            },
            now,
        );
        for i in 0..72 {
            overlay.push_level((i as f32 / 72.0).sin().abs());
        }

        let (w, h) = renderer.surface_size(&overlay);
        let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");

        let start = Instant::now();
        for i in 0..60 {
            renderer.geometry(&mut pixmap, &overlay, now, i as f32 * 0.016);
        }
        let geometry = start.elapsed() / 60;

        let start = Instant::now();
        for _ in 0..60 {
            renderer.labels(&mut pixmap, &overlay, now);
        }
        let labels = start.elapsed() / 60;

        let start = Instant::now();
        for i in 0..60 {
            renderer.draw(&mut pixmap, &overlay, now, i as f32 * 0.016);
        }
        let ring_only = start.elapsed() / 60;

        overlay.apply(
            &Event::PipelineStarted {
                transcriber: Some("openai".to_owned()),
                sinks: (0..4)
                    .map(|id| PlannedSink {
                        id,
                        name: format!("callback{id}"),
                        kind: "command".to_owned(),
                        requires_text: false,
                    })
                    .collect(),
            },
            now,
        );
        let (w, h) = renderer.surface_size(&overlay);
        let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");

        let start = Instant::now();
        for i in 0..60 {
            renderer.draw(&mut pixmap, &overlay, now, i as f32 * 0.016);
        }
        let with_panel = start.elapsed() / 60;

        println!("  geometry:   {geometry:?} per frame");
        println!("  labels:     {labels:?} per frame");
        println!("  ring only:  {ring_only:?} per frame");
        println!("  with panel: {with_panel:?} per frame");
        println!("  budget at 60fps is 16.6ms");
    }
}
