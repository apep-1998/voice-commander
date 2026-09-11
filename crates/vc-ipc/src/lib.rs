//! The Unix socket protocol spoken between the voice-commander daemon and its client.
//!
//! Shared by both sides so the request and response types cannot drift apart, and so the
//! daemon's integration tests drive it through the same client a keybind does.

pub mod client;
pub mod protocol;

pub use client::{Client, ClientError, EventStream};
pub use protocol::{
    ActivityState, Command, DaemonStatus, DeviceStatus, ErrorCode, Request, Response,
    PROTOCOL_VERSION,
};
