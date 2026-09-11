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
enum ConfigCommand {
    /// Write a commented starter configuration.
    Init {
        /// Where to write it. Defaults to $XDG_CONFIG_HOME/voice-commander/config.toml.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Load the configuration and report every problem with it.
    Check {
        #[arg(long, value_name = "DIR")]
        config_dir: Option<PathBuf>,
    },
    /// Print the fully resolved configuration, with every default filled in.
    Show {
        #[arg(long, value_name = "DIR")]
        config_dir: Option<PathBuf>,
    },
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
    /// Summarise your recordings, and say what the configuration should be.
    ///
    /// Reads the `session.json` files directly, so it works with the daemon stopped.
    Stats {
        /// Only sessions from the last N days.
        #[arg(long, value_name = "DAYS")]
        days: Option<u32>,
        /// Print the raw numbers as JSON.
        #[arg(long)]
        json: bool,
        /// Where recordings live. Defaults to $XDG_DATA_HOME/voice-commander.
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Configuration management.
    #[command(subcommand)]
    Config(ConfigCommand),
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
    if let Verb::Stats {
        days,
        json,
        data_dir,
    } = args.command
    {
        return stats(days, json, data_dir);
    }
    if let Verb::Config(command) = args.command {
        return config(command);
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
        Verb::Events { .. } | Verb::MicTest { .. } | Verb::Stats { .. } | Verb::Config(_) => {
            unreachable!("handled above")
        }
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

/// Summarise the recordings on disk.
///
/// Reads `session.json` files directly rather than asking the daemon, so it works with the
/// daemon stopped — and so it can be pointed at an archive copied from another machine.
fn stats(days: Option<u32>, json: bool, data_dir: Option<PathBuf>) -> Result<(), ClientError> {
    let root = data_dir.unwrap_or_else(vc_core::paths::data_dir);
    let cutoff =
        days.map(|days| time::OffsetDateTime::now_utc() - time::Duration::days(i64::from(days)));

    let mut sessions = Vec::new();
    let mut unreadable = 0usize;
    collect_sessions(&root.join("recordings"), &mut sessions, &mut unreadable, 0);
    if let Some(cutoff) = cutoff {
        sessions.retain(|session: &vc_core::SessionRecord| session.started_at >= cutoff);
    }

    if sessions.is_empty() {
        println!("no recordings found under {}", root.display());
        if unreadable > 0 {
            println!("({unreadable} file(s) could not be read)");
        }
        return Ok(());
    }

    let summary = vc_core::stats::Summary::from_sessions(&sessions);

    if json {
        println!("{summary:#?}");
        return Ok(());
    }

    println!("{} recordings", summary.sessions);
    println!(
        "  total audio:   {:.1} minutes",
        summary.total_audio_ms as f64 / 60_000.0
    );
    println!(
        "  typical length: {:.1}s (longest {:.1}s)",
        summary.median_duration_ms as f64 / 1000.0,
        summary.longest_ms as f64 / 1000.0
    );
    println!(
        "  continued:     {} ({}%)",
        summary.continued,
        summary.continued * 100 / summary.sessions.max(1)
    );
    if let Some(p90) = summary.resume_delay_p90() {
        println!("     you press again within {p90}ms, 90% of the time");
    }
    if let Some(p90) = summary.pre_roll_speech_p90() {
        println!(
            "  speech caught before the keypress: up to {p90}ms (window is {}ms)",
            summary.configured_pre_roll_ms.unwrap_or(0)
        );
    }
    if summary.watchdog_stops > 0 {
        println!("  cut off by the watchdog: {}", summary.watchdog_stops);
    }
    if summary.transcribed + summary.transcription_failures > 0 {
        println!(
            "  transcribed:   {} ({} failed, typically {}ms)",
            summary.transcribed, summary.transcription_failures, summary.median_transcription_ms
        );
    }
    if summary.sink_runs > 0 {
        println!(
            "  callbacks:     {} run, {} failed",
            summary.sink_runs, summary.sink_failures
        );
    }
    if unreadable > 0 {
        println!("  ({unreadable} session file(s) could not be read)");
    }

    let advice = summary.advice();
    if advice.is_empty() {
        println!("\nnothing to suggest — either it is well tuned, or there is not enough data yet");
    } else {
        println!("\nsuggestions:");
        for entry in advice {
            println!("  {}: {}", entry.setting, entry.message);
        }
    }
    Ok(())
}

/// Find every `session.json` under `dir`.
fn collect_sessions(
    dir: &std::path::Path,
    out: &mut Vec<vc_core::SessionRecord>,
    unreadable: &mut usize,
    depth: usize,
) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sessions(&path, out, unreadable, depth + 1);
        } else if path.file_name().is_some_and(|name| name == "session.json") {
            // A single unreadable file — written by an older version, or truncated by a
            // crash — must not stop the rest being summarised.
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
            {
                Some(record) => out.push(record),
                None => *unreadable += 1,
            }
        }
    }
}

/// Configuration subcommands.
fn config(command: ConfigCommand) -> Result<(), ClientError> {
    match command {
        ConfigCommand::Init { path, force } => {
            let path = path.unwrap_or_else(|| vc_core::paths::config_dir().join("config.toml"));
            if path.exists() && !force {
                eprintln!(
                    "{} already exists; pass --force to overwrite it",
                    path.display()
                );
                std::process::exit(1);
            }
            if let Some(parent) = path.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    eprintln!("creating {}: {error}", parent.display());
                    std::process::exit(1);
                }
            }
            if let Err(error) = std::fs::write(&path, vc_core::config::EXAMPLE_CONFIG) {
                eprintln!("writing {}: {error}", path.display());
                std::process::exit(1);
            }
            println!("wrote {}", path.display());
            println!("everything in it is optional — delete what you do not want to change");
            println!("then check it with `voice-commander config check`");
            Ok(())
        }
        ConfigCommand::Check { config_dir } => {
            let dir = config_dir.unwrap_or_else(vc_core::paths::config_dir);
            match vc_core::Config::load_from_dir(&dir) {
                Ok(loaded) => {
                    println!("{}: ok", dir.display());
                    println!(
                        "  profiles: {}",
                        loaded
                            .config
                            .profiles
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    for warning in &loaded.warnings {
                        println!("  warning: {warning}");
                    }
                    if loaded.warnings.is_empty() {
                        println!("  no warnings");
                    }
                    Ok(())
                }
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(6);
                }
            }
        }
        ConfigCommand::Show { config_dir } => {
            let dir = config_dir.unwrap_or_else(vc_core::paths::config_dir);
            match vc_core::Config::load_from_dir(&dir) {
                Ok(loaded) => {
                    match toml::to_string_pretty(&loaded.config) {
                        Ok(text) => println!("{text}"),
                        Err(error) => eprintln!("could not render the configuration: {error}"),
                    }
                    Ok(())
                }
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(6);
                }
            }
        }
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
