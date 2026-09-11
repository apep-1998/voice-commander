//! What a recording session is, and what is written down about it.

mod id;
mod record;

pub use id::SessionId;
pub use record::{
    rfc3339, AudioSummary, CaptureSummary, InputWarningKind, LevelSummary, Outcome, Segment,
    SessionRecord, SinkOutcome, SinkRecord, SkipReason, StopReason, TranscriptRecord,
};
