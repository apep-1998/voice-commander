//! Audio capture for voice-commander.
//!
//! Provides the `AudioSource` abstraction, the lock-free ring buffer that backs pre-roll
//! capture, level metering, and resampling to the rate the stored recording uses.
