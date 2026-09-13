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
use smithay_client_toolkit::shm::slot::SlotPool;
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
use crate::state::Overlay;

/// Where on screen the overlay sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Position {
    BottomCentre,
    TopCentre,
    Centre,
    BottomRight,
}

impl Position {
    fn anchor(self) -> Anchor {
        match self {
            Self::BottomCentre => Anchor::BOTTOM,
            Self::TopCentre => Anchor::TOP,
            Self::Centre => Anchor::empty(),
            Self::BottomRight => Anchor::BOTTOM | Anchor::RIGHT,
        }
    }

    fn margin(self, gap: i32) -> (i32, i32, i32, i32) {
        match self {
            Self::BottomCentre => (0, 0, gap, 0),
            Self::TopCentre => (gap, 0, 0, 0),
            Self::Centre => (0, 0, 0, 0),
            Self::BottomRight => (0, gap, gap, 0),
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
    pub overlay: Overlay,
    width: u32,
    height: u32,
    configured: bool,
    pub exit: bool,
    started: Instant,
}

impl Shell {
    /// Bring up the surface. Fails with a plain message if the compositor does not speak
    /// layer-shell, which is the one prerequisite worth naming.
    pub(crate) fn new(
        overlay: Overlay,
        renderer: Renderer,
        position: Position,
        gap: i32,
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
                width,
                height,
                configured: false,
                exit: false,
                started: Instant::now(),
            },
            connection,
            queue,
        ))
    }

    pub(crate) fn spin(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.started).as_secs_f32()
    }

    /// Resize the surface when the panel appears or disappears.
    fn resize_if_needed(&mut self) {
        let (width, height) = self.renderer.surface_size(&self.overlay);
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.layer.set_size(width, height);
        }
    }

    pub(crate) fn render(&mut self, qh: &QueueHandle<Self>, now: Instant) {
        if !self.configured {
            return;
        }
        self.resize_if_needed();

        // Hidden means an empty surface rather than a destroyed one: recreating the layer
        // surface on every keypress would cost a round trip at exactly the wrong moment.
        let (width, height) = (self.width, self.height);
        let stride = width as i32 * 4;

        // Rendered before the buffer is taken, so the pool's mutable borrow does not overlap
        // with reading the overlay to draw it.
        let Some(mut pixmap) = Pixmap::new(width, height) else {
            return;
        };
        if self.overlay.visible() {
            let spin = self.spin(now);
            self.renderer.draw(&mut pixmap, &self.overlay, now, spin);
        }

        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            return;
        };

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

        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        self.layer
            .wl_surface()
            .frame(qh, self.layer.wl_surface().clone());
        let _ = buffer.attach_to(self.layer.wl_surface());
        self.layer.commit();
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
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.render(qh, Instant::now());
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
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        self.configured = true;
        self.render(qh, Instant::now());
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
