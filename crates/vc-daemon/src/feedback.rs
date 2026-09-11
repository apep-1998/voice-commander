//! Which events reach anyone, and who they reach.
//!
//! `feedback.listening` and `feedback.processing` are independent switches over two distinct
//! groups of events: the ones that drive an indicator while the user is speaking, and the
//! ones that drive a progress panel while callbacks run. Turning one off suppresses those
//! events at the source rather than filtering them downstream — a user who does not want a
//! level meter should not be paying to produce twenty measurements a second for nobody.
//!
//! Everything outside those two groups always flows: daemon lifecycle and errors. Disabling
//! both indicators must not become a way to hide a failure.
//!
//! `session_finalized` belongs to the listening group, because it is that indicator's cue to
//! disappear. It follows the same switch — which does mean a daemon with feedback off emits
//! a quieter event stream. `session.json` is written either way, and that, not the event
//! log, is what `stats` reads.

use vc_core::config::FeedbackConfig;
use vc_core::event::Event;

/// Decides whether an event is worth emitting at all.
#[derive(Debug, Clone, Copy)]
pub struct Filter {
    enabled: bool,
    listening: bool,
    processing: bool,
}

impl Filter {
    pub fn new(config: &FeedbackConfig) -> Self {
        Self {
            enabled: config.enabled,
            listening: config.listening,
            processing: config.processing,
        }
    }

    /// Everything on, for tests and for the paths that have no configuration to consult.
    pub fn permissive() -> Self {
        Self {
            enabled: true,
            listening: true,
            processing: true,
        }
    }

    pub fn allows(self, event: &Event) -> bool {
        if event.is_listening_feedback() {
            return self.enabled && self.listening;
        }
        if event.is_processing_feedback() {
            return self.enabled && self.processing;
        }
        // Lifecycle and errors are not feedback and are never suppressed.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_core::event::Stage;

    fn config(enabled: bool, listening: bool, processing: bool) -> FeedbackConfig {
        FeedbackConfig {
            enabled,
            listening,
            processing,
            processing_linger_ms: 1_500,
            level_interval_ms: 50,
            presenters: Vec::new(),
        }
    }

    fn level() -> Event {
        Event::Level {
            rms_dbfs: -30.0,
            peak_dbfs: -10.0,
            speech: true,
            clipping: false,
        }
    }

    fn pipeline() -> Event {
        Event::PipelineStarted {
            transcriber: None,
            sinks: Vec::new(),
        }
    }

    fn failure() -> Event {
        Event::Error {
            stage: Stage::Capture,
            message: "no device".to_owned(),
        }
    }

    #[test]
    fn the_two_indicators_are_switched_independently() {
        let filter = Filter::new(&config(true, true, false));
        assert!(filter.allows(&level()));
        assert!(!filter.allows(&pipeline()));

        let filter = Filter::new(&config(true, false, true));
        assert!(!filter.allows(&level()));
        assert!(filter.allows(&pipeline()));
    }

    #[test]
    fn the_master_switch_covers_both() {
        let filter = Filter::new(&config(false, true, true));
        assert!(!filter.allows(&level()));
        assert!(!filter.allows(&pipeline()));
    }

    #[test]
    fn errors_are_never_suppressed() {
        // Disabling both indicators must not become a way to hide a failure.
        let filter = Filter::new(&config(false, false, false));
        assert!(filter.allows(&failure()));
        assert!(filter.allows(&Event::DaemonReady {
            version: "0.1.0".to_owned(),
            socket: "/tmp/s".into(),
        }));
    }

    #[test]
    fn a_finished_session_is_reported_even_with_feedback_off() {
        // It is what tells a log reader — or `events --follow` — that a recording exists.
        let filter = Filter::new(&config(false, false, false));
        assert!(
            !filter.allows(&Event::SessionFinalized {
                audio_path: "/a.wav".into(),
                total_ms: 100,
                segments: 1,
            }),
            "session_finalized drives the listening indicator's exit, so it follows that switch"
        );
    }
}
