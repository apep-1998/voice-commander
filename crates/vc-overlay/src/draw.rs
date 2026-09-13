//! Drawing the instrument.
//!
//! Everything is geometry except the readouts. The rotation of the arcs is the only purely
//! decorative element on the whole thing; every other mark encodes something the daemon
//! actually reported.

use std::time::Instant;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics as TextMetrics, Shaping, SwashCache};
use tiny_skia::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::state::{Overlay, Phase, RowStatus, Tone};
use crate::theme::{Metrics, Theme};

/// Width of the callback panel, when one is shown.
pub(crate) const PANEL_WIDTH: u32 = 300;
const PANEL_PAD: f32 = 14.0;
const ROW_HEIGHT: f32 = 21.0;

/// A run of text. Grouped so a call site reads as a description rather than a row of
/// unlabelled scalars.
#[derive(Clone, Copy)]
struct Text<'a> {
    size: f32,
    colour: Color,
    value: &'a str,
    centre: bool,
}

/// One arc segment. A struct rather than six positional scalars, which at a call site read
/// as an unlabelled tuple of magic numbers.
#[derive(Clone, Copy)]
struct Arc {
    r: f32,
    from: f32,
    len: f32,
    colour: Color,
    width: f32,
}

pub(crate) struct Renderer {
    pub theme: Theme,
    pub metrics: Metrics,
    fonts: FontSystem,
    cache: SwashCache,
}

impl Renderer {
    pub(crate) fn new(theme: Theme, metrics: Metrics) -> Self {
        Self {
            theme,
            metrics,
            fonts: FontSystem::new(),
            cache: SwashCache::new(),
        }
    }

    /// Total surface size. The panel is only allotted room when there is something in it.
    pub(crate) fn surface_size(&self, overlay: &Overlay) -> (u32, u32) {
        let width = if overlay.rows.is_empty() {
            self.metrics.size
        } else {
            self.metrics.size + PANEL_WIDTH
        };
        (width, self.metrics.size)
    }

    pub(crate) fn draw(&mut self, pixmap: &mut Pixmap, overlay: &Overlay, now: Instant, spin: f32) {
        pixmap.fill(Color::TRANSPARENT);

        let cx = self.metrics.size as f32 / 2.0;
        let cy = self.metrics.size as f32 / 2.0;

        self.ground(pixmap, overlay);
        self.brackets(pixmap, cx, cy);
        self.ticks(pixmap, cx, cy);
        self.arcs(pixmap, cx, cy, spin, overlay);
        self.cooldown(pixmap, cx, cy, overlay, now);
        self.waveform(pixmap, cx, cy, overlay);
        self.readout(pixmap, cx, cy, overlay, now);

        if !overlay.rows.is_empty() {
            self.panel(pixmap, overlay);
        }
    }

    /// The accent colour follows the input, so the whole instrument reports its state.
    fn accent(&self, overlay: &Overlay) -> Color {
        match overlay.tone {
            Tone::Clipping => self.theme.crit,
            Tone::Quiet => self.theme.amber,
            Tone::Normal => self.theme.hud,
        }
    }

    fn ground(&self, pixmap: &mut Pixmap, overlay: &Overlay) {
        // A disc rather than a rectangle: the ring should float, not sit in a box.
        let cx = self.metrics.size as f32 / 2.0;
        let mut paint = Paint::default();
        paint.set_color(self.theme.ground);
        paint.anti_alias = true;

        if let Some(circle) = PathBuilder::from_circle(cx, cx, self.metrics.tick_inner + 6.0) {
            pixmap.fill_path(
                &circle,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        if !overlay.rows.is_empty() {
            let rect = Rect::from_xywh(
                self.metrics.size as f32 - 4.0,
                28.0,
                PANEL_WIDTH as f32 - 16.0,
                self.metrics.size as f32 - 56.0,
            );
            if let Some(rect) = rect {
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
        }
    }

    fn stroke_path(&self, pixmap: &mut Pixmap, path: &tiny_skia::Path, colour: Color, width: f32) {
        let mut paint = Paint::default();
        paint.set_color(colour);
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            line_cap: LineCap::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
    }

    /// Corner framing — the HUD idiom, and it marks where the whole overlay sits.
    fn brackets(&self, pixmap: &mut Pixmap, cx: f32, cy: f32) {
        let r = self.metrics.bracket;
        let s = 13.0;
        for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let x = cx + sx * r;
            let y = cy + sy * r;
            let mut pb = PathBuilder::new();
            pb.move_to(x - sx * s, y);
            pb.line_to(x, y);
            pb.line_to(x, y - sy * s);
            if let Some(path) = pb.finish() {
                self.stroke_path(pixmap, &path, self.theme.rule, 1.0);
            }
        }
    }

    /// Graduations, with a longer mark every quarter, as on an instrument bezel.
    fn ticks(&self, pixmap: &mut Pixmap, cx: f32, cy: f32) {
        for i in 0..60 {
            let a = (i as f32 / 60.0) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let cardinal = i % 15 == 0;
            let outer = if cardinal {
                self.metrics.tick_outer + 4.0
            } else {
                self.metrics.tick_outer
            };
            let mut pb = PathBuilder::new();
            pb.move_to(
                cx + a.cos() * self.metrics.tick_inner,
                cy + a.sin() * self.metrics.tick_inner,
            );
            pb.line_to(cx + a.cos() * outer, cy + a.sin() * outer);
            if let Some(path) = pb.finish() {
                let colour = if cardinal {
                    self.theme.deep
                } else {
                    self.theme.rule
                };
                self.stroke_path(pixmap, &path, colour, if cardinal { 1.4 } else { 1.0 });
            }
        }
    }

    fn arc(&self, pixmap: &mut Pixmap, centre: (f32, f32), arc: Arc) {
        let (cx, cy) = centre;
        let Arc {
            r,
            from,
            len,
            colour,
            width,
        } = arc;
        // Approximated with a polyline: at these radii the segments are sub-pixel, and it
        // avoids hand-authoring bezier control points for every arc.
        let steps = ((len.abs() * 180.0).ceil() as usize).max(8);
        let mut pb = PathBuilder::new();
        for step in 0..=steps {
            let t = from + len * (step as f32 / steps as f32);
            let a = t * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let (x, y) = (cx + a.cos() * r, cy + a.sin() * r);
            if step == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        if let Some(path) = pb.finish() {
            self.stroke_path(pixmap, &path, colour, width);
        }
    }

    fn arcs(&self, pixmap: &mut Pixmap, cx: f32, cy: f32, spin: f32, overlay: &Overlay) {
        let accent = self.accent(overlay);
        let quiet = overlay.phase == Phase::Hidden;
        let alpha = |c: Color, a: f32| {
            let mut c = c;
            c.set_alpha(a * if quiet { 0.35 } else { 1.0 });
            c
        };

        let m = &self.metrics;
        let sets: [(f32, f32, f32, f32, f32, f32); 5] = [
            (m.arc_outer, 0.00, 0.62, 0.28, 1.0, 0.55),
            (m.arc_outer, 0.72, 0.16, 0.28, 1.0, 0.55),
            (m.arc_mid, 0.30, 0.34, -0.17, 1.6, 0.85),
            (m.arc_mid, 0.80, 0.08, -0.17, 1.6, 0.85),
            (m.arc_inner, 0.10, 0.22, 0.42, 1.0, 0.40),
        ];
        for (r, from, len, speed, width, a) in sets {
            self.arc(
                pixmap,
                (cx, cy),
                Arc {
                    r,
                    from: from + spin * speed,
                    len,
                    colour: alpha(accent, a),
                    width,
                },
            );
        }
    }

    /// How much of the continuation window is left — the reason release is not the end.
    fn cooldown(&self, pixmap: &mut Pixmap, cx: f32, cy: f32, overlay: &Overlay, now: Instant) {
        if overlay.phase != Phase::Cooling {
            return;
        }
        let frac = overlay.cooldown_fraction(now);
        if frac <= 0.0 {
            return;
        }
        self.arc(
            pixmap,
            (cx, cy),
            Arc {
                r: self.metrics.cooldown,
                from: 0.0,
                len: frac,
                colour: self.theme.deep,
                width: 2.5,
            },
        );
    }

    /// Spikes radiating from the centre, newest at the top running clockwise — so the shape
    /// is the last few seconds of the user's voice.
    fn waveform(&self, pixmap: &mut Pixmap, cx: f32, cy: f32, overlay: &Overlay) {
        let accent = self.accent(overlay);
        let bars = overlay.levels.len();
        for i in 0..bars {
            let idx = (overlay.head + bars - i) % bars;
            let v = overlay.levels[idx];
            let a = (i as f32 / bars as f32) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            let len = 2.0 + v * self.metrics.wave_max;

            let mut pb = PathBuilder::new();
            pb.move_to(
                cx + a.cos() * self.metrics.wave_base,
                cy + a.sin() * self.metrics.wave_base,
            );
            pb.line_to(
                cx + a.cos() * (self.metrics.wave_base + len),
                cy + a.sin() * (self.metrics.wave_base + len),
            );
            if let Some(path) = pb.finish() {
                // Fade with age, so the most recent spikes are the brightest.
                let age = 1.0 - i as f32 / bars as f32;
                let mut colour = if v > 0.12 { accent } else { self.theme.dim };
                colour.set_alpha(0.18 + age * 0.82);
                self.stroke_path(pixmap, &path, colour, 2.0);
            }
        }
    }

    // ── text ────────────────────────────────────────────────────────────────

    fn text(&mut self, pixmap: &mut Pixmap, at: (f32, f32), style: Text<'_>) {
        let (x, y) = at;
        let Text {
            size,
            colour,
            value,
            centre,
        } = style;
        if value.is_empty() {
            return;
        }
        let mut buffer = Buffer::new(&mut self.fonts, TextMetrics::new(size, size * 1.25));
        buffer.set_size(&mut self.fonts, Some(600.0), Some(size * 2.0));
        // Whatever technical face the system has; the fallback chain handles the rest.
        let attrs = Attrs::new().family(Family::Name("Chakra Petch"));
        buffer.set_text(&mut self.fonts, value, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);

        let width: f32 = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max);
        let origin_x = if centre { x - width / 2.0 } else { x };

        let (r, g, b, a) = (
            (colour.red() * 255.0) as u8,
            (colour.green() * 255.0) as u8,
            (colour.blue() * 255.0) as u8,
            (colour.alpha() * 255.0) as u8,
        );

        buffer.draw(
            &mut self.fonts,
            &mut self.cache,
            cosmic_text::Color::rgba(r, g, b, a),
            |gx, gy, gw, gh, gcolour| {
                if gcolour.a() == 0 {
                    return;
                }
                let mut paint = Paint::default();
                paint.set_color(Color::from_rgba8(
                    gcolour.r(),
                    gcolour.g(),
                    gcolour.b(),
                    gcolour.a(),
                ));
                if let Some(rect) = Rect::from_xywh(
                    origin_x + gx as f32,
                    y + gy as f32,
                    gw.max(1) as f32,
                    gh.max(1) as f32,
                ) {
                    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
                }
            },
        );
    }

    fn readout(&mut self, pixmap: &mut Pixmap, cx: f32, cy: f32, overlay: &Overlay, now: Instant) {
        let accent = self.accent(overlay);

        let primary = match overlay.phase {
            Phase::Listening => overlay
                .elapsed(now)
                .map(|d| {
                    let secs = d.as_secs();
                    format!("{}:{:02}", secs / 60, secs % 60)
                })
                .unwrap_or_else(|| "0:00".to_owned()),
            Phase::Cooling => format!(
                "{:.1}s",
                overlay.cooldown_fraction(now) * overlay.cooldown_total.as_secs_f32()
            ),
            // Nothing useful to count while callbacks run, and a placeholder reads as a
            // value that failed to load.
            Phase::Working => String::new(),
            Phase::Done => overlay.summary.clone().unwrap_or_else(|| "done".to_owned()),
            Phase::Hidden => String::new(),
        };

        let size = if overlay.phase == Phase::Done {
            15.0
        } else {
            30.0
        };
        self.text(
            pixmap,
            (cx, cy - size * 0.9),
            Text {
                size,
                colour: accent,
                value: &primary,
                centre: true,
            },
        );

        let status = overlay.status.to_uppercase();
        self.text(
            pixmap,
            (cx, cy + 8.0),
            Text {
                size: 10.0,
                colour: self.theme.text_dim,
                value: &status,
                centre: true,
            },
        );

        let detail = overlay.detail.clone();
        self.text(
            pixmap,
            (cx, cy + 24.0),
            Text {
                size: 9.0,
                colour: self.theme.dim,
                value: &detail,
                centre: true,
            },
        );
    }

    fn panel(&mut self, pixmap: &mut Pixmap, overlay: &Overlay) {
        let left = self.metrics.size as f32 + 6.0;
        let mut y = 40.0;

        // The hairline that ties the panel back to the ring — the annotation idiom.
        let mut pb = PathBuilder::new();
        pb.move_to(
            self.metrics.size as f32 - 26.0,
            self.metrics.size as f32 / 2.0,
        );
        pb.line_to(left - 8.0, self.metrics.size as f32 / 2.0);
        if let Some(path) = pb.finish() {
            self.stroke_path(pixmap, &path, self.theme.dim, 1.0);
        }

        let heading = match &overlay.transcriber {
            Some(name) => format!("PIPELINE · {}", name.to_uppercase()),
            None => "PIPELINE".to_owned(),
        };
        self.text(
            pixmap,
            (left + PANEL_PAD, y),
            Text {
                size: 9.0,
                colour: self.theme.deep,
                value: &heading,
                centre: false,
            },
        );
        y += 18.0;

        let mut pb = PathBuilder::new();
        pb.move_to(left + PANEL_PAD, y - 4.0);
        pb.line_to(left + PANEL_WIDTH as f32 - PANEL_PAD - 16.0, y - 4.0);
        if let Some(path) = pb.finish() {
            self.stroke_path(pixmap, &path, self.theme.rule, 1.0);
        }
        y += 4.0;

        for row in &overlay.rows {
            // Three outcomes, three marks. A dimmed row always says why.
            let (mark, colour) = match row.status {
                RowStatus::Planned => ("o", self.theme.dim),
                RowStatus::Running => ("*", self.theme.hud),
                RowStatus::Ok => ("+", self.theme.ok),
                RowStatus::Failed => ("x", self.theme.crit),
                RowStatus::Skipped => ("-", self.theme.dim),
            };
            let name_colour = match row.status {
                RowStatus::Planned | RowStatus::Skipped => self.theme.dim,
                RowStatus::Running => self.theme.hud,
                _ => self.theme.text,
            };

            self.text(
                pixmap,
                (left + PANEL_PAD, y),
                Text {
                    size: 13.0,
                    colour,
                    value: mark,
                    centre: false,
                },
            );
            let name = row.name.clone();
            self.text(
                pixmap,
                (left + PANEL_PAD + 16.0, y),
                Text {
                    size: 13.0,
                    colour: name_colour,
                    value: &name,
                    centre: false,
                },
            );
            let detail = row.detail.clone();
            self.text(
                pixmap,
                (left + PANEL_PAD + 130.0, y + 1.0),
                Text {
                    size: 10.0,
                    colour: self.theme.dim,
                    value: &detail,
                    centre: false,
                },
            );

            y += ROW_HEIGHT;
        }

        if let Some(summary) = &overlay.summary {
            y += 6.0;
            let summary = summary.to_uppercase();
            self.text(
                pixmap,
                (left + PANEL_PAD, y),
                Text {
                    size: 9.0,
                    colour: self.theme.deep,
                    value: &summary,
                    centre: false,
                },
            );
        }
    }
}
