//! `voice-commander` — the client a keybind runs.
//!
//! This binary exists to send one short message to the daemon and exit. Its startup cost
//! sits directly in the path between pressing the key and capturing audio, so it stays
//! deliberately dependency-light.

fn main() {
    println!(
        "voice-commander {} (event schema v{})",
        env!("CARGO_PKG_VERSION"),
        vc_core::EVENT_SCHEMA_VERSION
    );
}
