//! Retrying things that talk to the outside world.
//!
//! Used by transcribers and callbacks alike. The interesting decision is not how to back off
//! but *what is worth retrying*: a network blip deserves a second attempt, and a script that
//! exited non-zero because it decided to does not — running it again just does the wrong
//! thing twice.

use std::time::Duration;

use tracing::debug;
use vc_core::config::RetryConfig;

/// What an attempt decided.
#[derive(Debug)]
pub enum RetryOutcome<T, E> {
    /// Done.
    Done(T),
    /// Failed in a way another attempt might survive.
    Retry(E),
    /// Failed in a way another attempt will not change.
    Fatal(E),
}

/// Run `attempt` until it succeeds, gives up, or runs out of attempts.
///
/// Backoff doubles, which for the default of two attempts 500ms apart means a transient
/// failure costs half a second and a persistent one is not hammered.
///
/// Returns the error from the final attempt along with how many attempts were made — the
/// count goes into `session.json`, because "this webhook succeeded on the third try, every
/// time" is exactly the sort of thing worth being able to notice later.
pub async fn retry<T, E, F, Fut>(
    policy: &RetryConfig,
    label: &str,
    mut attempt: F,
) -> (Result<T, E>, u32)
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = RetryOutcome<T, E>>,
{
    let total = policy.attempts.max(1);
    let mut backoff = Duration::from_millis(policy.backoff_ms);
    let mut last: Option<E> = None;

    for number in 1..=total {
        match attempt(number).await {
            RetryOutcome::Done(value) => return (Ok(value), number),
            RetryOutcome::Fatal(error) => return (Err(error), number),
            RetryOutcome::Retry(error) => {
                last = Some(error);
                if number < total {
                    debug!(label, attempt = number, of = total, ?backoff, "retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
            }
        }
    }

    // The loop always runs at least once, so `last` is set whenever we get here.
    match last {
        Some(error) => (Err(error), total),
        None => unreachable!("retry ran zero attempts"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn policy(attempts: u32, backoff_ms: u64) -> RetryConfig {
        RetryConfig {
            attempts,
            backoff_ms,
        }
    }

    #[tokio::test]
    async fn a_success_on_the_first_try_costs_one_attempt() {
        let (result, attempts) = retry(&policy(3, 1), "test", |_| async {
            RetryOutcome::Done::<_, ()>("ok")
        })
        .await;

        assert_eq!(result, Ok("ok"));
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_works() {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&calls);

        let (result, attempts) = retry(&policy(5, 1), "test", move |_| {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                    RetryOutcome::Retry("connection refused")
                } else {
                    RetryOutcome::Done("ok")
                }
            }
        })
        .await;

        assert_eq!(result, Ok("ok"));
        assert_eq!(attempts, 3, "the attempt count goes into session.json");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_fatal_failure_is_not_retried() {
        // Running a script again that exited non-zero because it decided to just does the
        // wrong thing twice.
        let calls = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&calls);

        let (result, attempts) = retry(&policy(5, 1), "test", move |_| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RetryOutcome::Fatal::<(), _>("401 unauthorized")
            }
        })
        .await;

        assert_eq!(result, Err("401 unauthorized"));
        assert_eq!(attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a bad key is still bad");
    }

    #[tokio::test]
    async fn the_last_error_is_what_comes_back() {
        let (result, attempts) = retry(&policy(3, 1), "test", |number| async move {
            RetryOutcome::Retry::<(), _>(format!("attempt {number} failed"))
        })
        .await;

        assert_eq!(result, Err("attempt 3 failed".to_owned()));
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn one_attempt_means_no_retrying() {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&calls);

        let (_, attempts) = retry(&policy(1, 1), "test", move |_| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                RetryOutcome::Retry::<(), _>("nope")
            }
        })
        .await;

        assert_eq!(attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_zero_attempt_policy_still_tries_once() {
        // Configuration rejects this, but a caller constructing it directly should not get
        // silence in return for a nonsensical number.
        let (result, attempts) = retry(&policy(0, 1), "test", |_| async {
            RetryOutcome::Done::<_, ()>(1)
        })
        .await;
        assert_eq!(result, Ok(1));
        assert_eq!(attempts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_between_attempts() {
        // Paused time, so the assertion is about the schedule rather than about how fast the
        // test machine happens to be.
        let started = tokio::time::Instant::now();
        let (_, attempts) = retry(&policy(3, 100), "test", |_| async {
            RetryOutcome::Retry::<(), _>("nope")
        })
        .await;

        assert_eq!(attempts, 3);
        // 100ms before the second attempt, 200ms before the third.
        assert_eq!(started.elapsed(), Duration::from_millis(300));
    }
}
