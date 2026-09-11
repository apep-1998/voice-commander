//! Session timing: how long a recording may run, and what happens in the window after the
//! key is released.

use serde::{Deserialize, Serialize};

/// Limits on a single recording session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    /// After the key is released, how long to wait before finalising. Pressing the key again
    /// inside this window continues the same recording instead of starting a new one.
    pub cooldown_ms: u32,
    /// Force-stop a single uninterrupted recording after this long.
    ///
    /// This is not a nicety. A release keybind can be missed — release the modifier before
    /// the key and the compositor never fires it — and without this the daemon would record
    /// until the disk filled.
    pub max_recording_secs: u32,
    /// Force-finalise a session after this much total audio, across all its segments.
    pub max_total_secs: u32,
}

/// What to do with the audio between a release and the press that continues it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapMode {
    /// Keep the audio recorded during the gap, producing one continuous take. Only possible
    /// when the device stayed open; falls back to [`GapMode::Drop`] in
    /// [`CaptureMode::OnDemand`](super::CaptureMode::OnDemand).
    Keep,
    /// Concatenate the segments and discard the gap entirely.
    Drop,
    /// Concatenate the segments separated by digital silence, which nudges some
    /// speech-to-text models into treating them as distinct utterances.
    Silence,
}

/// How a recording continued during the cooldown window is stitched together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationConfig {
    pub gap: GapMode,
    /// Length of the inserted silence when `gap = "silence"`.
    pub silence_ms: u32,
    /// How many times one session may be continued before it is finalised regardless.
    pub max_segments: u32,
}

/// Whether a keybind holds to talk or toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerMode {
    /// Recording runs between a `start` and a `stop`, i.e. while the key is held. Needs both
    /// a press and a release binding.
    #[default]
    PushToTalk,
    /// Each `toggle` flips the state. Needs only a single press binding.
    Toggle,
}
