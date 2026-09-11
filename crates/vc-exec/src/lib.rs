//! Running the user's programs, and the retry and timeout policy around anything external.
//!
//! Shared by transcribers and callbacks because they need exactly the same things: spawn
//! something the user wrote, give it a deadline it cannot outlive, and decide whether a
//! failure is worth a second attempt. Keeping one implementation means a fix to either
//! applies to both.
//!
//! Nothing here goes through a shell. Commands are argv, expanded per argument, so a
//! transcript that came out of a microphone cannot become a second command.

pub mod command;
pub mod retry;

pub use command::{run, which, CommandOutput, CommandSpec, ExecError, RealRunner, Runner};
pub use retry::{retry, RetryOutcome};
