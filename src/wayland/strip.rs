//! Layer-shell surfaces: the 1 px barrier strip, and a probe-only "sensor" surface.
//!
//! Strip rules, all from niri's source:
//! - Overlay layer, so it is above windows, fullscreen and the overview for hit-testing.
//! - `exclusive_zone = -1`: with 0, other surfaces' exclusive zones (bar, DMS spacers) would
//!   push the strip off the real screen edge.
//! - Anchored to three edges with an explicit 1 px thickness; margins keep it off the corners
//!   (niri's hot corner wins over Overlay surfaces at its corner pixel anyway).
//! - A transparent `wl_shm` buffer: without a buffer the surface gets no input, and niri only
//!   offers single-pixel-buffer in tests.
//! - `keyboard_interactivity = none` until a grab makes it exclusive.

use std::os::fd::{AsFd, OwnedFd};

use anyhow::{Context, Result, bail};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::protocol::{wl_buffer::WlBuffer, wl_shm, wl_shm_pool::WlShmPool, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::Layer,
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use super::{WlEvent, WlState};

pub type SurfId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// Component of a relative motion that pushes out through this edge (positive = outward).
    pub fn outward(self, dx: f64, dy: f64) -> f64 {
        match self {
            Edge::Left => -dx,
            Edge::Right => dx,
            Edge::Top => -dy,
            Edge::Bottom => dy,
        }
    }

    /// Component of a relative motion along this edge.
    pub fn along(self, dx: f64, dy: f64) -> f64 {
        match self {
            Edge::Left | Edge::Right => dy,
            Edge::Top | Edge::Bottom => dx,
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfKind {
    Strip(Edge),
    /// Probe-only surface next to the strip, used to measure where a release lands.
    Sensor,
}

pub struct LayerSurf {
    pub kind: SurfKind,
    /// Output global name.
    #[allow(dead_code)]
    pub output: u32,
    pub surface: WlSurface,
    pub layer: ZwlrLayerSurfaceV1,
    /// Global logical position of the surface's (0,0).
    pub origin: (f64, f64),
    pub configured: Option<(u32, u32)>,
    pub keyboard_exclusive: bool,
    buffer: Option<ShmBuffer>,
}

struct ShmBuffer {
    buffer: WlBuffer,
    pool: WlShmPool,
    _fd: OwnedFd,
    size: (u32, u32),
}

impl ShmBuffer {
    fn destroy(self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// Where along an output edge a strip goes, in output-local logical coordinates.
#[derive(Clone, Copy, Debug)]
pub struct StripSpec {
    pub output: u32,
    pub edge: Edge,
    /// Start and end along the edge (end exclusive), output-local.
    pub start: i32,
    pub end: i32,
}

impl WlState {
    /// Creates a barrier strip. It becomes usable after its first configure
    /// ([`WlEvent::SurfaceConfigured`]).
    pub fn create_strip(&mut self, spec: StripSpec) -> Result<SurfId> {
        let rect = self.output_rect(spec.output)?;
        let len = if spec.edge.is_vertical() { rect.height } else { rect.width };
        let (start, end) = (spec.start.max(0), spec.end.min(len));
        if end - start < 1 {
            bail!("strip span {start}..{end} is empty");
        }
        let before = start as u32;
        let after = (len - end) as u32;
        let (anchor, size, margins, origin) = match spec.edge {
            Edge::Left => (
                Anchor::Left | Anchor::Top | Anchor::Bottom,
                (1, 0),
                (before, 0, after, 0),
                (rect.x as f64, (rect.y + start) as f64),
            ),
            Edge::Right => (
                Anchor::Right | Anchor::Top | Anchor::Bottom,
                (1, 0),
                (before, 0, after, 0),
                ((rect.x + rect.width - 1) as f64, (rect.y + start) as f64),
            ),
            Edge::Top => (
                Anchor::Top | Anchor::Left | Anchor::Right,
                (0, 1),
                (0, after, 0, before),
                ((rect.x + start) as f64, rect.y as f64),
            ),
            Edge::Bottom => (
                Anchor::Bottom | Anchor::Left | Anchor::Right,
                (0, 1),
                (0, after, 0, before),
                ((rect.x + start) as f64, (rect.y + rect.height - 1) as f64),
            ),
        };
        self.create_layer_surf(
            SurfKind::Strip(spec.edge),
            spec.output,
            "layercapture-barrier",
            anchor,
            size,
            margins,
            origin,
        )
    }

    /// Probe-only: a surface `width` px wide, `offset` px from the left edge, spanning
    /// `start..end`. With offset 1 it sits right next to a left strip and reports where the
    /// pointer lands after a release.
    pub fn create_left_sensor(&mut self, output: u32, offset: u32, width: u32, start: i32, end: i32) -> Result<SurfId> {
        let rect = self.output_rect(output)?;
        let (start, end) = (start.max(0), end.min(rect.height));
        self.create_layer_surf(
            SurfKind::Sensor,
            output,
            "layercapture-sensor",
            Anchor::Left | Anchor::Top | Anchor::Bottom,
            (width, 0),
            (start as u32, 0, (rect.height - end) as u32, offset),
            ((rect.x + offset as i32) as f64, (rect.y + start) as f64),
        )
    }

    fn output_rect(&self, output: u32) -> Result<super::outputs::Rect> {
        self.outputs
            .get(&output)
            .and_then(|o| o.logical)
            .context("output has no logical geometry")
    }

    #[allow(clippy::too_many_arguments)]
    fn create_layer_surf(
        &mut self,
        kind: SurfKind,
        output: u32,
        namespace: &str,
        anchor: Anchor,
        size: (u32, u32),
        (top, right, bottom, left): (u32, u32, u32, u32),
        origin: (f64, f64),
    ) -> Result<SurfId> {
        let compositor = self.globals.compositor.clone().context("no wl_compositor")?;
        let layer_shell = self.globals.layer_shell.clone().context("no layer shell")?;
        let wl_output = self.outputs.get(&output).context("unknown output")?.wl_output.clone();
        let id = self.alloc_surf_id();
        let surface = compositor.create_surface(&self.qh, ());
        let layer = layer_shell.get_layer_surface(
            &surface,
            Some(&wl_output),
            Layer::Overlay,
            namespace.to_owned(),
            &self.qh,
            id,
        );
        layer.set_anchor(anchor);
        layer.set_size(size.0, size.1);
        layer.set_margin(top as i32, right as i32, bottom as i32, left as i32);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        self.surfaces.insert(
            id,
            LayerSurf {
                kind,
                output,
                surface,
                layer,
                origin,
                configured: None,
                keyboard_exclusive: false,
                buffer: None,
            },
        );
        Ok(id)
    }

    pub fn destroy_surface(&mut self, id: SurfId) {
        if let Some(s) = self.surfaces.remove(&id) {
            if self.grab.as_ref().is_some_and(|g| g.surf == id) {
                // The grab objects reference this surface; drop them first.
                self.release_all(None);
            }
            if self.seat.pointer_focus == Some(id) {
                self.seat.pointer_focus = None;
            }
            s.layer.destroy();
            s.surface.destroy();
            if let Some(b) = s.buffer {
                b.destroy();
            }
        }
    }

    fn attach_buffer(&mut self, id: SurfId, width: u32, height: u32) -> Result<()> {
        let shm = self.globals.shm.clone().context("no wl_shm")?;
        let qh = self.qh.clone();
        let s = self.surfaces.get_mut(&id).context("unknown surface")?;
        if s.buffer.as_ref().is_some_and(|b| b.size == (width, height)) {
            return Ok(());
        }
        let stride = width.checked_mul(4).context("buffer too wide")?;
        let len = stride.checked_mul(height).context("buffer too large")?;
        // A zero-filled ARGB8888 buffer is fully transparent.
        let fd = memfd_create("layercapture-strip", MemfdFlags::CLOEXEC)?;
        ftruncate(&fd, len as u64)?;
        let pool = shm.create_pool(fd.as_fd(), len as i32, &qh, ());
        let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, wl_shm::Format::Argb8888, &qh, ());
        if let Some(old) = s.buffer.replace(ShmBuffer { buffer, pool, _fd: fd, size: (width, height) }) {
            old.destroy();
        }
        let b = &s.buffer.as_ref().unwrap().buffer;
        s.surface.attach(Some(b), 0, 0);
        s.surface.damage_buffer(0, 0, width as i32, height as i32);
        Ok(())
    }

    /// Sets the committed keyboard interactivity of a surface (the caller commits).
    pub fn set_keyboard_exclusive(&mut self, id: SurfId, exclusive: bool) {
        if let Some(s) = self.surfaces.get_mut(&id) {
            s.layer.set_keyboard_interactivity(if exclusive {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::None
            });
            s.keyboard_exclusive = exclusive;
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, SurfId> for WlState {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &SurfId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                let (w, h) = (width.max(1), height.max(1));
                if let Err(e) = state.attach_buffer(*id, w, h) {
                    tracing::error!("surface {id}: attaching buffer: {e:#}");
                    return;
                }
                if let Some(s) = state.surfaces.get_mut(id) {
                    s.surface.commit();
                    let first = s.configured.is_none();
                    s.configured = Some((w, h));
                    if first {
                        state.push(WlEvent::SurfaceConfigured { surf: *id, width: w, height: h });
                    }
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.push(WlEvent::SurfaceClosed { surf: *id }),
            _ => {}
        }
    }
}
