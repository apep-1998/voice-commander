//! `voice-commanderd` — the long-lived daemon.
//!
//! It owns the audio device, the pre-roll buffer, session state and the pipeline. Keeping
//! all of that resident is what makes a keypress start recording immediately rather than
//! paying for device setup every time.

use std::path::PathBuf;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "voice-commanderd",
    version,
    about = "The voice-commander daemon"
)]
struct Args {
    /// Configuration directory. Defaults to `$XDG_CONFIG_HOME/voice-commander`.
    #[arg(long, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Control socket. Defaults to `$XDG_RUNTIME_DIR/voice-commander.sock`.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Log filter, e.g. `debug` or `vc_daemon=trace,vc_audio=debug`.
    #[arg(long, value_name = "FILTER")]
    log: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Running as a systemd user service means stderr is already the journal, so plain
    // formatting without timestamps avoids every line carrying two of them.
    let in_journal = std::env::var_os("JOURNAL_STREAM").is_some();
    let filter = EnvFilter::try_from_env("VOICE_COMMANDER_LOG")
        .or_else(|_| EnvFilter::try_new(args.log.as_deref().unwrap_or("info")))?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if in_journal {
        builder.without_time().init();
    } else {
        builder.init();
    }

    let config_dir = args.config_dir.unwrap_or_else(vc_core::paths::config_dir);
    let socket = vc_ipc::client::resolve_socket_path(args.socket);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let daemon = vc_daemon::Daemon::new(&config_dir, socket)?;
        daemon.run(termination()).await
    })
}

/// Resolves when the service manager asks us to stop.
///
/// Both signals are handled, because `systemctl stop` sends SIGTERM and a terminal sends
/// SIGINT — and a daemon holding a microphone should let go of it either way.
async fn termination() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "cannot listen for SIGTERM");
            return;
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "cannot listen for SIGINT");
            return;
        }
    };

    tokio::select! {
        _ = term.recv() => {}
        _ = interrupt.recv() => {}
    }
}
