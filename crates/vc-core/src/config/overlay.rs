//! The on-screen indicator.
//!
//! Everything here is a knob someone might reasonably want to turn, which is why it is
//! configuration rather than constants: where the ring sits, how big it is, how hard it
//! works, how calm it looks, and what colour it is.

use serde::{Deserialize, Serialize};

/// Where on screen the overlay sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    /// Above where a bar would sit. Visible without covering what you are dictating into.
    #[default]
    Bottom,
    Top,
    Centre,
    BottomRight,
    BottomLeft,
}

/// An `#rrggbb` or `#rrggbbaa` colour.
///
/// Stored as the text the user wrote so `config show` gives their own value back rather than
/// a decomposed struct, and parsed on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hex(pub String);

impl Hex {
    /// Parse to `(r, g, b, a)`. Returns `None` for anything malformed, so a typo produces a
    /// configuration error naming the key rather than a silently black overlay.
    pub fn parse(&self) -> Option<(u8, u8, u8, u8)> {
        let text = self.0.strip_prefix('#')?;
        let byte = |i: usize| u8::from_str_radix(text.get(i..i + 2)?, 16).ok();
        match text.len() {
            6 => Some((byte(0)?, byte(2)?, byte(4)?, 255)),
            8 => Some((byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => None,
        }
    }
}

impl From<&str> for Hex {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// How the overlay moves.
///
/// The defaults are deliberately unhurried. An earlier version drew one spike per level
/// event and judged the warning colour on each sample, which was accurate and exhausting to
/// sit under — it jittered at twenty hertz and changed colour between syllables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayMotion {
    /// Radians per second the swell travels when nothing is being said. A slow drift, so a
    /// silent microphone still reads as alive rather than as a frozen picture.
    pub wave_speed: f32,
    /// Radians per second it travels at full voice.
    ///
    /// The speed follows the envelope between the two, which is most of what makes the ring
    /// feel like it is responding to you: height alone says how loud, and speed says
    /// *something is happening right now*.
    pub wave_speed_voice: f32,
    /// How quickly the ring rises to your voice, 0 to 1 per frame.
    pub attack: f32,
    /// How slowly it subsides. Much smaller than `attack`, so it settles like water rather
    /// than snapping shut between words.
    pub release: f32,
    /// Rotation rate of the decorative arcs. `0` stops them entirely.
    pub arc_speed: f32,
    /// Below this the input is reported as too quiet.
    pub quiet_enter_dbfs: f32,
    /// Above this it is reported as fine again.
    ///
    /// Two thresholds with a gap between them, not one. A single threshold makes the colour
    /// flip back and forth as ordinary speech crosses it.
    pub quiet_leave_dbfs: f32,
}

impl Default for OverlayMotion {
    fn default() -> Self {
        Self {
            wave_speed: 0.55,
            wave_speed_voice: 3.4,
            attack: 0.28,
            release: 0.04,
            arc_speed: 1.0,
            quiet_enter_dbfs: -48.0,
            quiet_leave_dbfs: -42.0,
        }
    }
}

/// What the overlay is made of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayColours {
    /// The primary instrument colour.
    pub hud: Hex,
    /// Cardinal graduations and secondary arcs.
    pub deep: Hex,
    /// Inactive ticks, and the swell when nothing is being said.
    pub dim: Hex,
    /// Frame lines.
    pub rule: Hex,
    /// Input too quiet.
    pub warn: Hex,
    /// Clipping, and failed callbacks.
    pub critical: Hex,
    /// Succeeded callbacks.
    pub ok: Hex,
    pub text: Hex,
    pub text_dim: Hex,
    /// Behind the ring. An alpha component here is what makes it translucent.
    pub ground: Hex,
    /// Behind the callback list. Opaque by default: it carries small text, and a bright
    /// window showing through makes it unreadable.
    pub panel_ground: Hex,
}

impl Default for OverlayColours {
    fn default() -> Self {
        Self {
            // Not pure cyan. `#00ffff` reads as generic neon; pulled toward sky and
            // desaturated, it reads as an instrument.
            hud: "#58d7ff".into(),
            deep: "#2b93b8".into(),
            dim: "#1c5a72".into(),
            rule: "#14293a".into(),
            warn: "#ffb64d".into(),
            critical: "#ff5f56".into(),
            ok: "#6fe3a8".into(),
            text: "#cfe9f5".into(),
            text_dim: "#6f93a6".into(),
            ground: "#05080cd2".into(),
            panel_ground: "#060a0f".into(),
        }
    }
}

/// Sizes, in logical pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayGeometry {
    /// Radius the swell oscillates about. Between the readout and the bezel.
    pub wave_base: f32,
    /// How far it rises at full voice. Large values fold the wave in over the readout.
    pub wave_height: f32,
    /// Width of the callback panel.
    pub panel_width: u32,
}

impl Default for OverlayGeometry {
    fn default() -> Self {
        Self {
            wave_base: 68.0,
            wave_height: 16.0,
            panel_width: 300.0 as u32,
        }
    }
}

/// Every field defaults, and so does every nested table: setting one colour should not mean
/// restating all eleven.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub position: OverlayPosition,
    /// Distance from that screen edge. Ignored when centred.
    pub margin: i32,
    /// Diameter of the ring.
    pub size: u32,
    /// Frames per second while something is on screen.
    ///
    /// The ring is drawn in software, so this is the main thing deciding what it costs. 30
    /// is smooth for a level meter and roughly halves the work.
    pub fps: u32,
    /// Font family for the readouts. Falls back to whatever the system has.
    pub font: String,
    pub motion: OverlayMotion,
    pub colours: OverlayColours,
    pub geometry: OverlayGeometry,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            position: OverlayPosition::Bottom,
            margin: 72,
            size: 240,
            fps: 60,
            font: "Chakra Petch".to_owned(),
            motion: OverlayMotion::default(),
            colours: OverlayColours::default(),
            geometry: OverlayGeometry::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_and_eight_digit_colours_both_parse() {
        assert_eq!(Hex::from("#58d7ff").parse(), Some((0x58, 0xd7, 0xff, 255)));
        assert_eq!(Hex::from("#05080cd2").parse(), Some((5, 8, 12, 0xd2)));
    }

    #[test]
    fn a_malformed_colour_is_rejected_rather_than_guessed() {
        // A typo should name the key in a config error, not silently paint something black.
        assert_eq!(Hex::from("58d7ff").parse(), None, "missing hash");
        assert_eq!(Hex::from("#58d7f").parse(), None, "wrong length");
        assert_eq!(Hex::from("#gggggg").parse(), None, "not hex");
        assert_eq!(Hex::from("").parse(), None);
    }

    #[test]
    fn the_defaults_are_all_valid_colours() {
        let colours = OverlayColours::default();
        for (name, hex) in [
            ("hud", &colours.hud),
            ("deep", &colours.deep),
            ("dim", &colours.dim),
            ("rule", &colours.rule),
            ("warn", &colours.warn),
            ("critical", &colours.critical),
            ("ok", &colours.ok),
            ("text", &colours.text),
            ("text_dim", &colours.text_dim),
            ("ground", &colours.ground),
            ("panel_ground", &colours.panel_ground),
        ] {
            assert!(
                hex.parse().is_some(),
                "{name} is not a valid colour: {}",
                hex.0
            );
        }
    }

    #[test]
    fn one_colour_can_be_changed_without_restating_the_rest() {
        // Requiring the whole table would make "I want a pink ring" a twelve-line edit.
        // Note the `r##`: a hex colour contains `"#`, which closes an `r#` string early.
        let partial: OverlayConfig = toml::from_str(
            r##"
size = 200
[colours]
hud = "#ff9de2"
[motion]
wave_speed = 0.35
"##,
        )
        .expect("a partial overlay table should parse");

        assert_eq!(partial.size, 200);
        assert_eq!(partial.colours.hud.0, "#ff9de2");
        assert_eq!(
            partial.colours.ok,
            OverlayColours::default().ok,
            "untouched"
        );
        assert!((partial.motion.wave_speed - 0.35).abs() < 1e-6);
        assert_eq!(partial.motion.attack, OverlayMotion::default().attack);
        assert_eq!(partial.position, OverlayPosition::Bottom, "untouched");
    }

    #[test]
    fn the_wave_speeds_up_rather_than_down_with_the_voice() {
        let motion = OverlayMotion::default();
        assert!(
            motion.wave_speed_voice > motion.wave_speed,
            "speaking should make it move more, not less"
        );
    }

    #[test]
    fn the_quiet_thresholds_leave_a_gap() {
        let motion = OverlayMotion::default();
        assert!(
            motion.quiet_leave_dbfs > motion.quiet_enter_dbfs,
            "without a gap the colour oscillates at the boundary"
        );
    }
}
