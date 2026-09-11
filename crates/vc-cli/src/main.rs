//! `voice-commander` — the client a keybind runs.
//!
//! This binary sends one short message to the daemon and exits. Its startup cost sits
//! directly between pressing the key and capturing audio, so it stays deliberately
//! dependency-light: no async runtime, no HTTP stack, and — on the path that matters — no
//! configuration parsing at all.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use vc_ipc::protocol::{Command as Cmd, Response};
use vc_ipc::{Client, ClientError};

/// How long to wait for the daemon. Generous for a local socket, and still short enough
/// that a wedged daemon cannot leave a keybind hanging.
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
#[command(
    name = "voice-commander",
    version,
    about = "Push-to-talk voice pipeline",
    long_about = "Bind `start` and `stop` to the press and release of one key:\n\n  \
                  bind  = SUPER, code:27, exec, voice-commander start --profile dictate\n  \
                  bindr = SUPER, code:27, exec, voice-commander stop  --profile dictate"
)]
struct Args {
    /// Control socket. Defaults to $VOICE_COMMANDER_SOCKET, then the XDG runtime directory.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Verb,
}

#[derive(Debug, Subcommand)]
enum Verb {
    /// Begin recording, or continue the session still in its cooldown window.
    Start {
        #[arg(long, short, default_value = "default")]
        profile: String,
    },
    /// Stop recording and open the cooldown window. Safe to run when nothing is recording.
    Stop {
        #[arg(long, short, default_value = "default")]
        profile: String,
    },
    /// Flip between recording and not, for `trigger = "toggle"` profiles.
    Toggle {
        #[arg(long, short, default_value = "default")]
        profile: String,
    },
    /// Discard whatever is in flight; nothing downstream runs.
    Cancel,
    /// Show what the daemon is doing.
    Status {
        /// Print the raw JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
    /// Print the event stream as newline-delimited JSON.
    Events {
        /// Keep the connection open. Without it, this exits at the first event.
        #[arg(long, short)]
        follow: bool,
        /// Only these event kinds, e.g. `--event recording_started --event level`.
        #[arg(long = "event", value_name = "KIND")]
        events: Vec<String>,
    },
    /// Re-read the configuration from disk.
    Reload,
    /// Record briefly and report what the microphone actually produced.
    ///
    /// Runs on its own, without the daemon, so it still works when nothing else does.
    MicTest {
        /// How long to record.
        #[arg(long, default_value_t = 3, value_name = "SECONDS")]
        seconds: u64,
        /// Device name substring. Defaults to the configured profile's device.
        #[arg(long, value_name = "NAME")]
        device: Option<String>,
        /// Which profile's capture settings to use.
        #[arg(long, short, default_value = "default")]
        profile: String,
        /// Configuration directory.
        #[arg(long, value_name = "DIR")]
        config_dir: Option<PathBuf>,
    },
    /// Check whether the daemon is up.
    Ping,
    /// Ask the daemon to exit.
    Shutdown,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("voice-commander: {error}");
            if let ClientError::NotRunning { path } = &error {
                eprintln!(
                    "  start it with `systemctl --user start voice-commander`, \
                     or run `voice-commanderd` directly\n  (socket: {})",
                    path.display()
                );
            }
            // A distinct code per failure so a wrapper script can branch without parsing
            // this text.
            ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
        }
    }
}

fn run(args: Args) -> Result<(), ClientError> {
    let socket = vc_ipc::client::resolve_socket_path(args.socket);

    if let Verb::Events { follow, events } = args.command {
        return stream_events(&socket, follow, events);
    }
    if let Verb::MicTest {
        seconds,
        device,
        profile,
        config_dir,
    } = args.command
    {
        return mic_test(seconds, device, &profile, config_dir);
    }

    let want_json = matches!(args.command, Verb::Status { json: true });
    let command = match args.command {
        Verb::Start { profile } => Cmd::Start { profile },
        Verb::Stop { profile } => Cmd::Stop { profile },
        Verb::Toggle { profile } => Cmd::Toggle { profile },
        Verb::Cancel => Cmd::Cancel,
        Verb::Status { .. } => Cmd::Status,
        Verb::Reload => Cmd::Reload,
        Verb::Ping => Cmd::Ping,
        Verb::Shutdown => Cmd::Shutdown,
        Verb::Events { .. } | Verb::MicTest { .. } => unreachable!("handled above"),
    };
    let response = Client::connect(&socket, TIMEOUT)?.send(command)?;
    report(&response, want_json);
    Ok(())
}

fn report(response: &Response, want_json: bool) {
    match response {
        // Start and stop are bound to keys and run dozens of times an hour. Printing
        // anything on success would fill the journal with noise nobody reads.
        Response::Accepted { .. } => {}
        Response::Pong { version, protocol } => {
            println!("voice-commanderd {version} (protocol v{protocol})");
        }
        Response::Reloaded { warnings } => {
            if warnings.is_empty() {
                println!("configuration reloaded");
            } else {
                println!("configuration reloaded with {} warning(s):", warnings.len());
                for warning in warnings {
                    println!("  - {warning}");
                }
            }
        }
        Response::Status(status) => print_status(status, want_json),
        Response::Subscribed => {}
        Response::Error { message, .. } => eprintln!("{message}"),
    }
}

fn print_status(status: &vc_ipc::DaemonStatus, want_json: bool) {
    if want_json {
        if let Ok(json) = serde_json::to_string_pretty(status) {
            println!("{json}");
        }
        return;
    }

    use vc_ipc::protocol::ActivityState;
    let activity = match status.activity {
        ActivityState::Idle => "idle".to_owned(),
        ActivityState::Recording {
            segment,
            elapsed_ms,
        } => format!(
            "recording (segment {segment}, {:.1}s)",
            elapsed_ms as f64 / 1000.0
        ),
        ActivityState::Cooling { remaining_ms } => {
            format!("cooling down ({remaining_ms}ms left to continue)")
        }
        ActivityState::Processing => "processing".to_owned(),
    };

    println!("voice-commanderd {}", status.version);
    println!("  state:     {activity}");
    println!("  uptime:    {}s", status.uptime_secs);
    match &status.device {
        Some(device) => println!(
            "  device:    {} ({} Hz, {} ch, {}ms of pre-roll buffered)",
            device.name, device.sample_rate, device.channels, device.pre_roll_available_ms
        ),
        None => println!("  device:    closed"),
    }
    if let Some(profile) = &status.profile {
        println!("  profile:   {profile}");
    }
    println!("  profiles:  {}", status.profiles.join(", "));
    println!("  socket:    {}", status.socket.display());
    if status.config_warnings > 0 {
        println!(
            "  config:    {} warning(s) — run `voice-commander reload` to see them",
            status.config_warnings
        );
    }
}

/// Check the microphone without involving the daemon.
///
/// Deliberately standalone: this is what a user runs when recording produces nothing, and it
/// would be useless if it needed the very thing that is not working.
fn mic_test(
    seconds: u64,
    device: Option<String>,
    profile_name: &str,
    config_dir: Option<PathBuf>,
) -> Result<(), ClientError> {
    let dir = config_dir.unwrap_or_else(vc_core::paths::config_dir);
    let loaded = match vc_core::Config::load_from_dir(&dir) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("configuration: {error}");
            std::process::exit(6);
        }
    };

    let Some(profile) = loaded.config.profiles.get(profile_name) else {
        eprintln!("no profile named {profile_name:?}");
        std::process::exit(3);
    };

    let selector = match device {
        Some(name) => vc_core::config::DeviceSelector::Match(name),
        None => profile.capture.device.clone(),
    };

    println!("recording for {seconds}s — say something...");
    let report = match vc_audio::mic_test(
        &selector,
        loaded.config.audio.sample_rate,
        &loaded.config.levels,
        Duration::from_secs(seconds),
    ) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(5);
        }
    };

    println!();
    println!("  device:  {}", report.device.name);
    println!(
        "  format:  {} Hz, {} channel(s)",
        report.device.sample_rate, report.device.channels
    );
    println!("  peak:    {:.1} dBFS", report.peak_dbfs);
    println!("  average: {:.1} dBFS", report.mean_rms_dbfs);
    println!(
        "  speech:  {:.1}s of {:.1}s",
        report.speech_ms as f64 / 1000.0,
        report.duration.as_secs_f64()
    );
    if report.clipped_samples > 0 {
        println!("  clipped: {} samples", report.clipped_samples);
    }
    if report.dropped_samples > 0 {
        println!(
            "  dropped: {} samples (this machine could not keep up)",
            report.dropped_samples
        );
    }
    println!();
    println!("{}", report.verdict());
    Ok(())
}

fn stream_events(
    socket: &std::path::Path,
    follow: bool,
    events: Vec<String>,
) -> Result<(), ClientError> {
    let stream = Client::connect(socket, TIMEOUT)?.subscribe(events)?;
    let mut stdout = std::io::stdout().lock();

    for line in stream {
        let line = line.map_err(|source| ClientError::Io {
            path: socket.to_owned(),
            source,
        })?;
        // Flushing every line is the point: this output is piped into status bars and
        // indicators that react as events happen, and block buffering would hold an event
        // back until the next few arrived.
        if writeln!(stdout, "{line}").is_err() || stdout.flush().is_err() {
            return Ok(()); // The other end of the pipe went away, e.g. `| head`.
        }
        if !follow {
            return Ok(());
        }
    }
    Ok(())
}
