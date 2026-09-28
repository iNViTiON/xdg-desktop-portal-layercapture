//! Outputs and their logical geometry (wl_output + zxdg_output_v1).
//!
//! Zones, barriers and cursor positions all use the global logical coordinate space that
//! xdg-output reports (the same space niri uses for pointer positions).

use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use super::{WlEvent, WlState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct Output {
    pub wl_output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    pub name: Option<String>,
    /// Committed logical geometry (after `wl_output.done`).
    pub logical: Option<Rect>,
    pending_pos: Option<(i32, i32)>,
    pending_size: Option<(i32, i32)>,
}

impl Output {
    pub fn new(wl_output: WlOutput) -> Self {
        Self {
            wl_output,
            xdg_output: None,
            name: None,
            logical: None,
            pending_pos: None,
            pending_size: None,
        }
    }

    pub fn ensure_xdg_output(&mut self, mgr: &ZxdgOutputManagerV1, name: u32, qh: &QueueHandle<WlState>) {
        if self.xdg_output.is_none() {
            self.xdg_output = Some(mgr.get_xdg_output(&self.wl_output, qh, name));
        }
    }

    pub fn destroy(self) {
        if let Some(x) = self.xdg_output {
            x.destroy();
        }
        if self.wl_output.version() >= 3 {
            self.wl_output.release();
        }
    }

    fn commit(&mut self) -> bool {
        let (Some((x, y)), Some((width, height))) = (self.pending_pos, self.pending_size) else {
            return false;
        };
        let rect = Rect { x, y, width, height };
        let changed = self.logical != Some(rect);
        self.logical = Some(rect);
        changed
    }
}

use wayland_client::Proxy;

impl Dispatch<WlOutput, u32> for WlState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state.outputs.get_mut(name) else { return };
        match event {
            wl_output::Event::Name { name } => out.name = Some(name),
            wl_output::Event::Done => {
                if out.commit() {
                    tracing::debug!(output = ?out.name, logical = ?out.logical, "output geometry");
                    state.push(WlEvent::OutputsChanged);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for WlState {
    fn event(
        state: &mut Self,
        xdg: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state.outputs.get_mut(name) else { return };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => out.pending_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                out.pending_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } if out.name.is_none() => out.name = Some(name),
            // xdg_output v3 relies on wl_output.done; older versions send their own done.
            zxdg_output_v1::Event::Done if xdg.version() < 3 => {
                if out.commit() {
                    state.push(WlEvent::OutputsChanged);
                }
            }
            _ => {}
        }
    }
}
