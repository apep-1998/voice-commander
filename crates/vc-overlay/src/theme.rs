//! Colours and sizes.
//!
//! Kept in one place so the whole look can be retuned without hunting through the drawing
//! code — and so it can become a config file later without moving anything.

use tiny_skia::Color;

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

pub(crate) struct Theme {
    /// The panel ground. Translucent: a HUD floats over the desktop, it does not cover it.
    pub ground: Color,
    /// The primary instrument colour.
    pub hud: Color,
    /// Brighter cardinal graduations and secondary arcs.
    pub deep: Color,
    /// Inactive ticks and room-tone spikes.
    pub dim: Color,
    /// Frame lines.
    pub rule: Color,
    /// Input too quiet.
    pub amber: Color,
    /// Clipping, and failed callbacks.
    pub crit: Color,
    /// Succeeded callbacks.
    pub ok: Color,
    pub text: Color,
    pub text_dim: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ground: Color::from_rgba8(5, 8, 12, 210),
            // Not pure cyan: #00ffff reads as generic neon. Pulled toward sky and
            // desaturated, it reads as an instrument.
            hud: rgb(0x58, 0xd7, 0xff),
            deep: rgb(0x2b, 0x93, 0xb8),
            dim: rgb(0x1c, 0x5a, 0x72),
            rule: rgb(0x14, 0x29, 0x3a),
            amber: rgb(0xff, 0xb6, 0x4d),
            crit: rgb(0xff, 0x5f, 0x56),
            ok: rgb(0x6f, 0xe3, 0xa8),
            text: rgb(0xcf, 0xe9, 0xf5),
            text_dim: rgb(0x6f, 0x93, 0xa6),
        }
    }
}

/// Geometry of the ring, in logical pixels.
pub(crate) struct Metrics {
    pub size: u32,
    pub bracket: f32,
    pub tick_inner: f32,
    pub tick_outer: f32,
    pub arc_outer: f32,
    pub arc_mid: f32,
    pub arc_inner: f32,
    pub cooldown: f32,
    pub wave_base: f32,
    pub wave_max: f32,
    /// How many spikes the waveform is drawn from — also how much history it holds.
    pub bars: usize,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            size: 240,
            bracket: 116.0,
            tick_inner: 84.0,
            tick_outer: 91.0,
            arc_outer: 108.0,
            arc_mid: 99.0,
            arc_inner: 62.0,
            cooldown: 76.0,
            wave_base: 46.0,
            wave_max: 34.0,
            bars: 72,
        }
    }
}
