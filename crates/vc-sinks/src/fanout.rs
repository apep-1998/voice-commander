//! Running a profile's callbacks.
//!
//! The shape of this is dictated by what an indicator needs to draw. The full plan is known
//! before anything runs, each callback resolves independently, and every outcome is a tick, a
//! cross, or a greyed-out row with a reason — never a silent absence.
//!
//! Kept out of the daemon so it can be tested without a socket, a microphone or a runtime
//! full of other things.

use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};
use vc_core::config::{OnError, RetryConfig, SinkMode};
use vc_core::session::{SinkOutcome, SinkRecord, SkipReason};
use vc_exec::{retry, RetryOutcome};

use crate::{Sink, SinkContext};

/// One callback and the policy around it.
pub struct Planned {
    /// Position in the fan-out. This, not the name, is what an indicator correlates on — the
    /// same sink may be listed twice in a profile, and two rows sharing an identifier cannot
    /// be told apart.
    pub id: u32,
    pub sink: Arc<dyn Sink>,
    pub on_error: OnError,
    pub retry: RetryConfig,
}

impl std::fmt::Debug for Planned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planned")
            .field("id", &self.id)
            .field("name", &self.sink.name())
            .finish_non_exhaustive()
    }
}

/// What happened, as it happens.
#[derive(Debug, Clone)]
pub enum Progress {
    Started { id: u32, name: String },
    Finished { record: SinkRecord },
}

/// Run every callback and report each one's outcome.
///
/// `skip` is set when the session has no usable transcript, and carries the reason: a greyed
/// row without one is indistinguishable from a callback that silently never ran.
pub async fn run(
    planned: Vec<Planned>,
    context: Arc<SinkContext>,
    mode: SinkMode,
    skip: Option<SkipReason>,
    report: Arc<dyn Fn(Progress) + Send + Sync>,
) -> Vec<SinkRecord> {
    match mode {
        SinkMode::Sequential => {
            let mut records = Vec::with_capacity(planned.len());
            let mut abandon: Option<SkipReason> = None;

            for entry in planned {
                if let Some(reason) = abandon {
                    records.push(skipped(&entry, reason, &report));
                    continue;
                }
                let record = one(&entry, &context, skip, &report).await;
                // Only sequential mode can honour `fail_session` meaningfully: in parallel
                // the others have already started, and cancelling work that may already have
                // had side effects is worse than letting it finish.
                if entry.on_error == OnError::FailSession && !record.outcome.is_ok() {
                    abandon = Some(SkipReason::EarlierSinkFailed);
                }
                records.push(record);
            }
            records
        }
        SinkMode::Parallel => {
            // One task each, so callbacks genuinely run at the same time rather than merely
            // interleaving — a webhook waiting on the network must not hold up a local
            // script. Everything they share is behind an `Arc`, which is why the futures can
            // be `'static` and spawned at all.
            let mut set = tokio::task::JoinSet::new();
            for entry in planned {
                let context = Arc::clone(&context);
                let report = Arc::clone(&report);
                set.spawn(async move { one(&entry, &context, skip, &report).await });
            }

            let mut records = Vec::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(record) => records.push(record),
                    // A panicking callback is a bug here, not in the user's script, but the
                    // session should still finish with whatever else succeeded.
                    Err(error) => warn!(%error, "a callback task did not complete"),
                }
            }
            // Stable order, so an indicator's rows do not shuffle as they resolve.
            records.sort_by_key(|record| record.id);
            records
        }
    }
}

/// Run one callback, honouring its skip condition and retry policy.
async fn one(
    entry: &Planned,
    context: &SinkContext,
    skip: Option<SkipReason>,
    report: &Arc<dyn Fn(Progress) + Send + Sync>,
) -> SinkRecord {
    if let Some(reason) = skip {
        if entry.sink.requires_text() {
            return skipped(entry, reason, report);
        }
    }

    report(Progress::Started {
        id: entry.id,
        name: entry.sink.name().to_owned(),
    });

    let started = Instant::now();
    let (result, attempts) = retry(&entry.retry, entry.sink.name(), |_| async {
        match entry.sink.deliver(context).await {
            Ok(()) => RetryOutcome::Done(()),
            Err(error) if error.is_transient() => RetryOutcome::Retry(error),
            Err(error) => RetryOutcome::Fatal(error),
        }
    })
    .await;

    let latency_ms = started.elapsed().as_millis() as u64;
    let outcome = match result {
        Ok(()) => {
            info!(sink = entry.sink.name(), latency_ms, "callback succeeded");
            SinkOutcome::Ok
        }
        Err(error) => {
            // Logged at warn rather than error: with the default `on_error = "ignore"` the
            // session as a whole is still fine, and the user is told through the event stream.
            warn!(sink = entry.sink.name(), %error, latency_ms, "callback failed");
            SinkOutcome::Failed {
                error: error.to_string(),
            }
        }
    };

    let record = SinkRecord {
        id: entry.id,
        name: entry.sink.name().to_owned(),
        kind: entry.sink.kind().to_owned(),
        outcome,
        latency_ms,
        attempts,
    };
    report(Progress::Finished {
        record: record.clone(),
    });
    record
}

fn skipped(
    entry: &Planned,
    reason: SkipReason,
    report: &Arc<dyn Fn(Progress) + Send + Sync>,
) -> SinkRecord {
    let record = SinkRecord {
        id: entry.id,
        name: entry.sink.name().to_owned(),
        kind: entry.sink.kind().to_owned(),
        outcome: SinkOutcome::Skipped { reason },
        latency_ms: 0,
        attempts: 0,
    };
    report(Progress::Finished {
        record: record.clone(),
    });
    record
}

/// Tally the outcomes, for `pipeline_finished`.
pub fn tally(records: &[SinkRecord]) -> (usize, usize, usize) {
    let mut ok = 0;
    let mut failed = 0;
    let mut skipped = 0;
    for record in records {
        match record.outcome {
            SinkOutcome::Ok => ok += 1,
            SinkOutcome::Failed { .. } => failed += 1,
            SinkOutcome::Skipped { .. } => skipped += 1,
        }
    }
    (ok, failed, skipped)
}
