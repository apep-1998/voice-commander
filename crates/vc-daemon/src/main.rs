//! `voice-commanderd` — the long-lived daemon.
//!
//! It owns the audio device, the pre-roll buffer, session state and the pipeline. Keeping
//! all of that in a resident process is what makes a keypress start recording immediately
//! rather than paying for device setup every time.

fn main() {
    println!(
        "voice-commanderd {} (event schema v{})",
        env!("CARGO_PKG_VERSION"),
        vc_core::EVENT_SCHEMA_VERSION
    );
}
