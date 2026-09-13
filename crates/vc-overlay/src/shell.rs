//! The Wayland surface.
//!
//! `wlr-layer-shell` is what makes this an overlay rather than a window: it floats above
//! everything, takes no keyboard focus, reserves no space in the layout, and the compositor
//! will not tile it. Dictation types into whatever window *is* focused, so an overlay that
//! stole focus would break the feature it exists to report on.

use std::time::Instant;

use anyhow::{Context, Result};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use tiny_skia::Pixmap;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

use smithay_client_toolkit::reexports::client as wayland_client;

use crate::draw::Renderer;
use crate::feed::Feed;
use crate::state::Overlay;

/// Where on screen the overlay sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Position {
    BottomCentre,
    TopCentre,
    Centre,
    BottomRight,
    BottomLeft,
}

impl Position {
    fn anchor(self) -> Anchor {
        match self {
            Self::BottomCentre => Anchor::BOTTOM,
            Self::TopCentre => Anchor::TOP,
            Self::Centre => Anchor::empty(),
            Self::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
            Self::BottomLeft => Anchor::BOTTOM | Anchor::LEFT,
        }
    }

    fn margin(self, gap: i32) -> (i32, i32, i32, i32) {
        match self {
            Self::BottomCentre => (0, 0, gap, 0),
            Self::TopCentre => (gap, 0, 0, 0),
            // No anchor means the compositor centres it; a margin would only shove it off
            // centre again, so it is deliberately ignored.
            Self::Centre => (0, 0, 0, 0),
            Self::BottomRight => (0, gap, gap, 0),
            Self::BottomLeft => (0, 0, gap, gap),
        }
    }
}

pub(crate) struct Shell {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    renderer: Renderer,
    pub(crate) overlay: Overlay,
    feed: Feed,
    /// Reused between frames. Allocating and zeroing a quarter of a megabyte sixty times a
    /// second is cheap enough to be invisible in a profile and pointless all the same.
    scratch: Option<Pixmap>,
    /// Reused across frames, with the size it was made at.
    ///
    /// Creating a `wl_buffer` per frame means allocating a protocol object, sending it and
    /// destroying it sixty times a second, which costs more than all the drawing put
    /// together. The size is stored beside it because a buffer that no longer matches the
    /// surface must be replaced, not written into — doing that writes rows at the wrong
    /// stride and paints the screen with streaks.
    buffer: Option<(Buffer, u32, u32)>,
    width: u32,
    height: u32,
    /// The size most recently asked of the compositor.
    requested: (u32, u32),
    configured: bool,
    pub(crate) exit: bool,
    started: Instant,
    /// Frame-rate reporting, when asked for. Cheap to leave in and the only way to tell a
    /// compositor problem from one of ours.
    frames: u32,
    last_report: Instant,
    report_fps: bool,
    /// True for the scripted demo, which produces its own events and so must keep ticking
    /// even before anything is on screen.
    pub(crate) animating: bool,
    /// Time between frames while something is moving.
    frame_budget: std::time::Duration,
    pub(crate) timer_ticks: u32,
}

impl Shell {
    /// Bring up the surface. Fails with a plain message if the compositor does not speak
    /// layer-shell, which is the one prerequisite worth naming.
    pub(crate) fn new(
        overlay: Overlay,
        renderer: Renderer,
        feed: Feed,
        position: Position,
        gap: i32,
        fps: u32,
    ) -> Result<(Self, Connection, wayland_client::EventQueue<Self>)> {
        let connection = Connection::connect_to_env()
            .context("cannot reach a Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let (globals, queue) = registry_queue_init(&connection)?;
        let qh: QueueHandle<Self> = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh)
            .context("the compositor does not expose wl_compositor")?;
        let layer_shell = LayerShell::bind(&globals, &qh).context(
            "this compositor does not support wlr-layer-shell, which is what lets an \
             overlay float above everything without taking focus (Hyprland, Sway and \
             river all do)",
        )?;
        let shm = Shm::bind(&globals, &qh).context("the compositor does not expose wl_shm")?;

        let surface = compositor.create_surface(&qh);
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("voice-commander"),
            None,
        );

        let (width, height) = renderer.surface_size(&overlay);
        layer.set_size(width, height);
        layer.set_anchor(position.anchor());
        let (top, right, bottom, left) = position.margin(gap);
        layer.set_margin(top, right, bottom, left);
        // The whole point: it must never take the keyboard, because dictation types into
        // whatever window actually has focus.
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Reserve nothing; windows keep their full area and the overlay floats over them.
        layer.set_exclusive_zone(-1);
        layer.commit();

        let pool = SlotPool::new((width * height * 4) as usize, &shm)
            .context("cannot allocate a shared-memory buffer")?;

        Ok((
            Self {
                registry: RegistryState::new(&globals),
                output: OutputState::new(&globals, &qh),
                shm,
                pool,
                layer,
                renderer,
                overlay,
                feed,
                scratch: None,
                buffer: None,
                width,
                height,
                requested: (width, height),
                configured: false,
                exit: false,
                started: Instant::now(),
                frames: 0,
                last_report: Instant::now(),
                report_fps: std::env::var_os("VOICE_COMMANDER_OVERLAY_FPS").is_some(),
                animating: false,
                frame_budget: std::time::Duration::from_micros(
                    1_000_000 / u64::from(fps.clamp(1, 240)),
                ),
                timer_ticks: 0,
            },
            connection,
            queue,
        ))
    }

    /// How long to wait before drawing again.
    ///
    /// At the refresh rate while there is something moving, and a lazy poll otherwise —
    /// an overlay meant to be left running via `exec-once` must cost nothing while the user
    /// is not talking to it.
    pub(crate) fn next_frame(&self) -> std::time::Duration {
        if self.overlay.visible() || self.animating {
            self.frame_budget
        } else {
            // Nothing is moving and nothing is on screen. This only has to be often enough
            // to notice the daemon saying a recording started.
            std::time::Duration::from_millis(200)
        }
    }

    pub(crate) fn spin(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.started).as_secs_f32()
    }

    /// Resize the surface when the panel appears or disappears.
    ///
    /// Compared against the size last *requested*, not the size last configured. The
    /// compositor answers `set_size` with a configure whether or not the value changed, and
    /// configuring triggers a render — so calling it unconditionally from the render path is
    /// a feedback loop that runs as fast as the machine allows. That is what "a little
    /// laggy" turned out to be: twenty times the intended frame rate, and a saturated core.
    fn resize_if_needed(&mut self) {
        let wanted = self.renderer.surface_size(&self.overlay);
        if wanted != self.requested {
            self.requested = wanted;
            // Only the request is made here. `self.width` and `self.height` are whatever the
            // compositor last configured, and drawing at any other size means handing it a
            // buffer it did not agree to.
            self.layer.set_size(wanted.0, wanted.1);
        }
    }

    /// Draw one frame and ask the compositor for the next.
    ///
    /// Called only from the frame callback, which is the compositor saying it is ready —
    /// so this runs at exactly the refresh rate, never faster and never twice for one
    /// vblank. An earlier version also rendered from the main loop and slept sixteen
    /// milliseconds afterwards, which queued two frame callbacks per cycle and put a sleep
    /// on top of the wait for vblank. It looked like lag because it was.
    pub(crate) fn render(&mut self, now: Instant) {
        if !self.configured {
            return;
        }

        // Everything time-driven happens here, so there is one clock and one place that
        // advances it.
        self.overlay.tick(now);
        for event in self.feed.poll(now) {
            self.overlay.apply(&event, now);
        }

        self.resize_if_needed();
        let (width, height) = (self.width, self.height);

        // Draw first: the pool's borrow below must not overlap with reading the overlay.
        let sized = self
            .scratch
            .as_ref()
            .is_some_and(|p| p.width() == width && p.height() == height);
        if !sized {
            self.scratch = Pixmap::new(width, height);
        }
        let Some(mut pixmap) = self.scratch.take() else {
            return;
        };

        if self.overlay.visible() {
            let spin = self.spin(now);
            self.renderer.draw(&mut pixmap, &self.overlay, now, spin);
        } else {
            // Hidden is an empty surface rather than a destroyed one: tearing the layer
            // surface down and rebuilding it on every keypress would cost a round trip at
            // exactly the wrong moment.
            pixmap.fill(tiny_skia::Color::TRANSPARENT);
        }

        if self.write_and_commit(&pixmap, width, height).is_none() {
            self.scratch = Some(pixmap);
            return;
        }
        self.scratch = Some(pixmap);

        self.count_frame(now);
    }

    /// Copy the frame into a shared buffer and put it on screen.
    ///
    /// The buffer is reused when the compositor has released it — `canvas` returning `None`
    /// is how it says otherwise, and is the signal to take a second one. Creating a fresh
    /// `wl_buffer` every frame instead costs more than all the drawing put together.
    fn write_and_commit(&mut self, pixmap: &Pixmap, width: u32, height: u32) -> Option<()> {
        let stride = width as i32 * 4;

        // Compared against the size the buffer was actually made at, not the size we last
        // asked for: those differ for the frame between requesting a resize and being
        // configured.
        let fits = self
            .buffer
            .as_ref()
            .is_some_and(|(_, w, h)| (*w, *h) == (width, height));
        if !fits {
            self.buffer = None;
        }

        // `canvas` returning None means the compositor is still showing it, which is the
        // signal to take a fresh one rather than draw over what is on screen.
        let usable = self
            .buffer
            .as_mut()
            .and_then(|(b, _, _)| b.canvas(&mut self.pool))
            .is_some();
        if !usable {
            let (buffer, _) = self
                .pool
                .create_buffer(
                    width as i32,
                    height as i32,
                    stride,
                    wl_shm::Format::Argb8888,
                )
                .ok()?;
            self.buffer = Some((buffer, width, height));
        }

        let (buffer, _, _) = self.buffer.as_mut()?;
        let canvas = buffer.canvas(&mut self.pool)?;

        // tiny-skia gives premultiplied RGBA; Wayland's ARGB8888 wants the bytes swapped.
        for (dst, src) in canvas
            .chunks_exact_mut(4)
            .zip(pixmap.data().chunks_exact(4))
        {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        let (buffer, _, _) = self.buffer.as_ref()?;
        buffer.attach_to(surface).ok()?;
        self.layer.commit();
        Some(())
    }

    fn count_frame(&mut self, now: Instant) {
        if !self.report_fps {
            return;
        }
        self.frames += 1;
        let since = now.saturating_duration_since(self.last_report);
        if since >= std::time::Duration::from_secs(1) {
            eprintln!(
                "{:.1} fps ({} renders, {} timer ticks)",
                f64::from(self.frames) / since.as_secs_f64(),
                self.frames,
                self.timer_ticks
            );
            self.frames = 0;
            self.timer_ticks = 0;
            self.last_report = now;
        }
    }
}

impl CompositorHandler for Shell {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // Deliberately empty. A frame callback means "you may draw", not "draw now" — and a
        // compositor answers it immediately for a surface it is not presenting, which turns
        // callback-driven rendering into a spin. The timer in `main` decides when to draw.
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Shell {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Shell {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let _ = qh;
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        // Deliberately does not draw. A configure records what the compositor decided; the
        // timer decides when to put something on screen. Drawing here means every commit can
        // provoke another configure, and the two chase each other as fast as the machine
        // allows — which is exactly what happened.
        self.configured = true;
    }
}

impl ShmHandler for Shell {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Shell {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(Shell);
delegate_output!(Shell);
delegate_shm!(Shell);
delegate_layer!(Shell);
delegate_registry!(Shell);
