//! Core types shared across the whole workspace: configuration, the session model, the
//! versioned event schema, and the traits that make transcription and callbacks pluggable.
//!
//! This crate deliberately has no I/O of its own beyond reading configuration files. It
//! defines *what* things are, so that `vc-daemon` can orchestrate them and `vc-audio`,
//! `vc-stt` and `vc-sinks` can implement them without depending on each other.

pub mod config;
pub mod paths;

pub use config::{Config, ConfigError, Loaded};

/// Wire-format version stamped on every emitted event.
///
/// The event stream is a public contract: external indicators — a bar module, a future
/// overlay — are written against it. Bump this only for a breaking change, and document the
/// change in `docs/events.md`.
pub const EVENT_SCHEMA_VERSION: u32 = 1;
