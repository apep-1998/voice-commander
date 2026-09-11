//! Core types shared across the whole workspace: configuration, the session model, the
//! versioned event schema, and the traits that make transcription and callbacks pluggable.
//!
//! This crate deliberately has no I/O of its own. It defines *what* things are so that
//! `vc-daemon` can orchestrate them and `vc-audio` / `vc-stt` / `vc-sinks` can implement
//! them without depending on each other.

/// Wire-format version stamped on every emitted event.
///
/// The event stream is a public contract: external indicators (a waybar module, a future
/// overlay) are written against it. Bump this only for a breaking change, and document the
/// change in `docs/events.md`.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_schema_version_is_pinned() {
        // A deliberate tripwire: bumping the version must be a conscious act that also
        // updates docs/events.md and any downstream consumers.
        assert_eq!(EVENT_SCHEMA_VERSION, 1);
    }
}
