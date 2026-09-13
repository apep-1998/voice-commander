//! Colours and sizes.
//!
//! Kept in one place so the whole look can be retuned without hunting through the drawing
//! code — and so it can become a config file later without moving anything.

use tiny_skia::Color;

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

pub(crate) struct Theme {
    /// Behind the ring. Translucent: a HUD floats over the desktop, it does not cover it.
    pub ground: Color,
    /// Behind the callback list. Opaque, unlike the ring's ground: this one carries
    /// 13px text, and a bright window showing through behind it is simply unreadable.
    pub panel_ground: Color,
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

impl Theme {
    /// Build from the configuration, falling back to the default for any colour that does
    /// not parse — validation reports those, and an overlay that refused to start over one
    /// bad hex string would be worse than one that starts looking slightly wrong.
    pub(crate) fn from_config(config: &vc_core::config::OverlayColours) -> Self {
        let fallback = Self::default();
        let pick = |hex: &vc_core::config::Hex, default: Color| {
            hex.parse()
                .map_or(default, |(r, g, b, a)| Color::from_rgba8(r, g, b, a))
        };

        Self {
            ground: pick(&config.ground, fallback.ground),
            panel_ground: pick(&config.panel_ground, fallback.panel_ground),
            hud: pick(&config.hud, fallback.hud),
            deep: pick(&config.deep, fallback.deep),
            dim: pick(&config.dim, fallback.dim),
            rule: pick(&config.rule, fallback.rule),
            amber: pick(&config.warn, fallback.amber),
            crit: pick(&config.critical, fallback.crit),
            ok: pick(&config.ok, fallback.ok),
            text: pick(&config.text, fallback.text),
            text_dim: pick(&config.text_dim, fallback.text_dim),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            ground: Color::from_rgba8(5, 8, 12, 210),
            panel_ground: Color::from_rgba8(6, 10, 15, 255),
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
    /// How much level history is kept. The ring is drawn from the envelope, not from these,
    /// but anything wanting the raw samples reads them.
    pub bars: usize,
    /// Rotation rate of the decorative arcs.
    pub arc_speed: f32,
    pub panel_width: u32,
    pub font: String,
}

impl Metrics {
    /// Sizes scale with the ring, so `--size 180` gives a smaller instrument rather than the
    /// same instrument with a gap round it.
    pub(crate) fn from_config(config: &vc_core::config::OverlayConfig) -> Self {
        let default = Self::default();
        let scale = config.size as f32 / default.size as f32;

        Self {
            size: config.size,
            bracket: default.bracket * scale,
            tick_inner: default.tick_inner * scale,
            tick_outer: default.tick_outer * scale,
            arc_outer: default.arc_outer * scale,
            arc_mid: default.arc_mid * scale,
            arc_inner: default.arc_inner * scale,
            cooldown: default.cooldown * scale,
            wave_base: config.geometry.wave_base * scale,
            wave_max: config.geometry.wave_height * scale,
            bars: default.bars,
            arc_speed: config.motion.arc_speed,
            panel_width: config.geometry.panel_width,
            font: config.font.clone(),
        }
    }
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
            // The swell lives in its own band, between the readout and the bezel. Given a
            // large amplitude it folds in over the text and reads as a blob rather than a
            // wave, so the base sits well out and the height stays modest.
            wave_base: 68.0,
            wave_max: 16.0,
            bars: 72,
            arc_speed: 1.0,
            panel_width: 300,
            font: "Chakra Petch".to_owned(),
        }
    }
}
