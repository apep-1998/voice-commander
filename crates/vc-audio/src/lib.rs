//! Audio capture for voice-commander.
//!
//! The shape here is deliberate: the backend's callback does nothing but hand samples to a
//! lock-free queue, and every decision — pre-roll, metering, resampling — happens on an
//! ordinary thread draining that queue. An audio callback that allocates, locks or logs will
//! eventually miss its deadline, and a missed deadline is a gap in the user's recording.
//!
//! Everything except the backend itself is therefore testable without a microphone, which is
//! why [`SyntheticSource`] exists and why no test in this crate needs sound hardware.

pub mod cpal_backend;
pub mod level;
pub mod mictest;
pub mod preroll;
pub mod resample;
pub mod source;

pub use cpal_backend::{CpalHost, CpalSource};
pub use level::{summary_warning, LevelMeter, LevelSnapshot, LevelTotals};
pub use mictest::{run as mic_test, MicReport};
pub use preroll::PreRollBuffer;
pub use resample::Resampler;
pub use source::{
    channel, AudioError, AudioHost, AudioSource, DeviceInfo, SampleSink, SampleSource,
    SyntheticSource,
};
