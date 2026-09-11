//! Presenters: the things that actually show a user what is happening.
//!
//! No graphical indicator ships. What ships is this — a socket every subscriber reads, a
//! command that can be any script, and desktop notifications — plus the guarantee that an
//! overlay written later has no more access to the daemon than any of them.
//!
//! Each runs as its own task subscribed to the event bus, so a slow or wedged presenter
//! cannot hold up the pipeline that produced the event. It falls behind and loses events
//! rather than stalling a recording.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};

use vc_core::config::{Config, PresenterKind};
use vc_core::Envelope;
use vc_exec::CommandSpec;

/// Start every presenter the configuration asks for, plus the event log.
///
/// Returns the tasks so shutdown can let them finish what they were writing.
pub fn start(
    config: &Config,
    data_dir: &Path,
    events: &broadcast::Sender<String>,
    stop: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = vec![tokio::spawn(log_events(
        data_dir.join("events.jsonl"),
        events.subscribe(),
        stop.clone(),
    ))];

    for name in &config.feedback.presenters {
        let Some(presenter) = config.presenters.get(name) else {
            continue; // Already reported by configuration validation.
        };

        match &presenter.kind {
            // Subscribers read from the socket directly; there is nothing to run.
            PresenterKind::Socket(_) => {
                debug!(
                    presenter = name,
                    "events are available on the control socket"
                );
            }
            PresenterKind::Command(command) => {
                info!(presenter = name, "starting command presenter");
                tasks.push(tokio::spawn(run_command(
                    name.clone(),
                    command.cmd.clone(),
                    command.env.clone(),
                    command.events.clone(),
                    events.subscribe(),
                    stop.clone(),
                )));
            }
            PresenterKind::Notify(notify) => {
                info!(presenter = name, "starting notification presenter");
                tasks.push(tokio::spawn(run_notify(
                    notify.per_sink,
                    events.subscribe(),
                    stop.clone(),
                )));
            }
        }
    }

    tasks
}

/// Append every event to `events.jsonl`.
///
/// The corpus for working out, weeks later, what the configuration should have been. Opened
/// once and appended to rather than reopened per event: this is on the path of twenty level
/// measurements a second.
async fn log_events(
    path: PathBuf,
    mut events: broadcast::Receiver<String>,
    mut stop: watch::Receiver<bool>,
) {
    if let Some(parent) = path.parent() {
        if let Err(error) = tokio::fs::create_dir_all(parent).await {
            warn!(%error, path = %parent.display(), "cannot create the log directory");
            return;
        }
    }

    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            warn!(%error, path = %path.display(), "cannot open the event log");
            return;
        }
    };

    loop {
        let line = tokio::select! {
            received = events.recv() => received,
            _ = stop.changed() => break,
        };

        match line {
            Ok(line) => {
                if file
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    warn!(path = %path.display(), "cannot write to the event log");
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                debug!(skipped, "the event log fell behind");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    // Flushed on the way out, so the last events of a session are not lost with the buffer.
    let _ = file.flush().await;
}

/// Run a command for each event, with the event JSON on standard input.
///
/// The shell-script path to a custom indicator. One process per event is not free, which is
/// why the `events` allow-list exists: a bar module that only cares about start and stop
/// should not be woken twenty times a second by level updates.
async fn run_command(
    name: String,
    cmd: Vec<String>,
    env: BTreeMap<String, String>,
    wanted: Vec<String>,
    mut events: broadcast::Receiver<String>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let line = tokio::select! {
            received = events.recv() => received,
            _ = stop.changed() => return,
        };

        let line = match line {
            Ok(line) => line,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                debug!(presenter = name, skipped, "presenter fell behind");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };

        if !wanted.is_empty() && !matches_kind(&line, &wanted) {
            continue;
        }

        let spec = CommandSpec::new(cmd.clone())
            .with_env(env.clone())
            .with_stdin(Some(line))
            // Short: an indicator that takes longer than this to react has already missed
            // the moment, and a slow one must not build up a backlog of processes.
            .with_timeout(Duration::from_secs(5));

        if let Err(error) = vc_exec::run(&spec).await {
            warn!(presenter = name, %error, "presenter command failed");
        }
    }
}

/// Raise desktop notifications for the coarse milestones.
///
/// Not a substitute for an indicator — notifications cannot show a level meter or a live
/// progress list — but it works on any desktop with nothing else installed, which is worth
/// having while no overlay exists.
async fn run_notify(
    per_sink: bool,
    mut events: broadcast::Receiver<String>,
    mut stop: watch::Receiver<bool>,
) {
    use vc_core::event::Event;

    loop {
        let line = tokio::select! {
            received = events.recv() => received,
            _ = stop.changed() => return,
        };

        let line = match line {
            Ok(line) => line,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };

        let Ok(envelope) = Envelope::from_ndjson(&line) else {
            continue;
        };

        let message = match &envelope.event {
            Event::InputWarning { kind, .. } => Some((format!("microphone: {kind:?}"), "critical")),
            Event::PipelineFinished {
                ok,
                failed,
                skipped,
                ..
            } => Some((
                format!("{ok} done, {failed} failed, {skipped} skipped"),
                if *failed > 0 { "critical" } else { "low" },
            )),
            Event::SinkFinished { name, outcome, .. } if per_sink => {
                Some((format!("{name}: {outcome:?}"), "low"))
            }
            Event::Error { stage, message } => Some((format!("{stage:?}: {message}"), "critical")),
            _ => None,
        };

        let Some((body, urgency)) = message else {
            continue;
        };

        let spec = CommandSpec::new(vec![
            "notify-send".to_owned(),
            "-u".to_owned(),
            urgency.to_owned(),
            "--".to_owned(),
            "voice-commander".to_owned(),
            body,
        ])
        .with_timeout(Duration::from_secs(5));

        if let Err(error) = vc_exec::run(&spec).await {
            warn!(%error, "notification presenter failed");
        }
    }
}

/// Whether an event line is one of the kinds a presenter asked for.
fn matches_kind(line: &str, wanted: &[String]) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("event")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|kind| wanted.iter().any(|want| want == &kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_allow_list_lets_everything_through() {
        // Filtering is opt-in: a presenter that says nothing wants everything.
        let line = r#"{"v":1,"event":"level","rms_dbfs":-30.0}"#;
        assert!(!matches_kind(line, &["recording_started".to_owned()]));
        assert!(matches_kind(line, &["level".to_owned()]));
    }

    #[test]
    fn a_malformed_line_matches_nothing_rather_than_everything() {
        assert!(!matches_kind("not json", &["level".to_owned()]));
        assert!(!matches_kind("{}", &["level".to_owned()]));
    }
}
