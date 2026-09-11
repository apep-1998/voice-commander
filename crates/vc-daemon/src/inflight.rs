//! Keeping track of pipelines that are still running.
//!
//! Transcription and callbacks run detached, so the daemon can accept the next recording
//! while the previous one is still uploading. The consequence is that shutting down naively
//! tears the runtime out from under them — which was observed losing the transcript and the
//! callback results from `session.json` even though the callbacks themselves had already
//! run. The recording of what happened is worth as much as the happening.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tracing::{debug, warn};

/// A counter of running pipelines that can be waited on.
#[derive(Debug, Clone, Default)]
pub struct InFlight {
    count: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pipeline. The returned guard decrements when dropped, so a task that
    /// panics still releases its slot rather than blocking shutdown forever.
    pub fn enter(&self) -> Guard {
        self.count.fetch_add(1, Ordering::SeqCst);
        Guard {
            count: Arc::clone(&self.count),
            idle: Arc::clone(&self.idle),
        }
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Wait until nothing is running, or until `grace` elapses.
    ///
    /// Bounded because a callback may legitimately be waiting on a 60-second webhook, and a
    /// user who asked the daemon to stop should not be made to wait for it.
    pub async fn drain(&self, grace: Duration) {
        if self.count() == 0 {
            return;
        }
        debug!(pending = self.count(), "waiting for pipelines to finish");

        let wait = async {
            while self.count() > 0 {
                self.idle.notified().await;
            }
        };

        if tokio::time::timeout(grace, wait).await.is_err() {
            warn!(
                pending = self.count(),
                ?grace,
                "giving up on pipelines that are still running; their results were not recorded"
            );
        }
    }
}

/// Released when a pipeline finishes, however it finishes.
#[derive(Debug)]
pub struct Guard {
    count: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn draining_returns_at_once_when_nothing_is_running() {
        let started = std::time::Instant::now();
        InFlight::new().drain(Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn draining_waits_for_a_running_pipeline() {
        let tracker = InFlight::new();
        let guard = tracker.enter();
        assert_eq!(tracker.count(), 1);

        let waiter = tracker.clone();
        let handle = tokio::spawn(async move { waiter.drain(Duration::from_secs(5)).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "it returned before the work was done"
        );

        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("draining should finish once the guard is released")
            .expect("task");
    }

    #[tokio::test]
    async fn draining_gives_up_after_the_grace_period() {
        // A callback may legitimately be waiting on a slow webhook, and a user who asked the
        // daemon to stop should not be made to wait for it.
        let tracker = InFlight::new();
        let _guard = tracker.enter();

        let started = std::time::Instant::now();
        tracker.drain(Duration::from_millis(100)).await;

        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_guard_is_released_even_if_its_task_panics() {
        let tracker = InFlight::new();
        let held = tracker.clone();

        let handle = tokio::spawn(async move {
            let _guard = held.enter();
            panic!("callbacks can have bugs too");
        });
        let _ = handle.await;

        assert_eq!(
            tracker.count(),
            0,
            "a panicking pipeline must not block shutdown"
        );
    }

    #[tokio::test]
    async fn several_pipelines_are_all_waited_for() {
        let tracker = InFlight::new();
        let guards: Vec<Guard> = (0..3).map(|_| tracker.enter()).collect();
        assert_eq!(tracker.count(), 3);

        let waiter = tracker.clone();
        let handle = tokio::spawn(async move { waiter.drain(Duration::from_secs(5)).await });

        for guard in guards {
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert!(!handle.is_finished());
            drop(guard);
        }

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("should finish")
            .expect("task");
    }
}
