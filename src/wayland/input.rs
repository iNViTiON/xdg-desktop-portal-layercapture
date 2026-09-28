//! Seat input: wl_pointer, zwp_relative_pointer_v1, wl_keyboard.
//!
//! The relative pointer is created with the pointer, not at grab time: the activation trigger
//! ("pressure" against the edge) needs relative motion while the pointer rests unlocked on the
//! strip, and niri sends it there with the unclamped delta.

use std::os::fd::OwnedFd;

use wayland_client::protocol::{
    wl_keyboard::{self, KeymapFormat, WlKeyboard},
    wl_pointer::{self, Axis, AxisSource, WlPointer},
    wl_seat::{self, Capability, WlSeat},
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
    self, ZwpRelativePointerV1,
};

use super::{SurfId, WlEvent, WlState};
use crate::keymap::Keymap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

/// Scroll information accumulated over one `wl_pointer.frame`. Index 0 = horizontal (x),
/// 1 = vertical (y), matching EIS `scroll(x, y)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AxisFrame {
    pub source: Option<AxisSource>,
    /// Continuous scroll distance (`wl_pointer.axis`).
    pub value: [f64; 2],
    pub has_value: [bool; 2],
    /// High-resolution wheel steps (`axis_value120`); 120 = one detent.
    pub value120: [i32; 2],
    /// `axis_stop` was sent for the axis (end of a finger/continuous scroll).
    pub stop: [bool; 2],
}

impl AxisFrame {
    fn is_empty(&self) -> bool {
        !self.has_value[0]
            && !self.has_value[1]
            && self.value120 == [0, 0]
            && !self.stop[0]
            && !self.stop[1]
    }
}

fn axis_index(axis: WEnum<Axis>) -> Option<usize> {
    match axis {
        WEnum::Value(Axis::HorizontalScroll) => Some(0),
        WEnum::Value(Axis::VerticalScroll) => Some(1),
        _ => None,
    }
}

#[derive(Default)]
pub struct SeatState {
    pub pointer: Option<WlPointer>,
    pub keyboard: Option<WlKeyboard>,
    pub relative: Option<ZwpRelativePointerV1>,
    pub cursor_shape: Option<WpCursorShapeDeviceV1>,
    /// Our surface that currently has pointer focus.
    pub pointer_focus: Option<SurfId>,
    /// Surface-local pointer position on `pointer_focus`.
    pub pointer_pos: (f64, f64),
    /// Serial of the latest `wl_pointer.enter` (for set_cursor / cursor shape).
    pub enter_serial: u32,
    /// Our surface that currently has keyboard focus.
    pub keyboard_focus: Option<SurfId>,
    axis: AxisFrame,
}

impl SeatState {
    pub fn drop_devices(&mut self) {
        if let Some(r) = self.relative.take() {
            r.destroy();
        }
        if let Some(c) = self.cursor_shape.take() {
            c.destroy();
        }
        if let Some(p) = self.pointer.take()
            && p.version() >= 3
        {
            p.release();
        }
        if let Some(k) = self.keyboard.take()
            && k.version() >= 3
        {
            k.release();
        }
        self.pointer_focus = None;
        self.keyboard_focus = None;
    }
}

impl Dispatch<WlSeat, ()> for WlState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event else {
            return;
        };
        let mut lost = false;
        if caps.contains(Capability::Pointer) {
            if state.seat.pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                if let Some(mgr) = &state.globals.relative_pointer_manager {
                    state.seat.relative = Some(mgr.get_relative_pointer(&pointer, qh, ()));
                }
                if let Some(mgr) = &state.globals.cursor_shape_manager {
                    state.seat.cursor_shape = Some(mgr.get_pointer(&pointer, qh, ()));
                }
                state.seat.pointer = Some(pointer);
            }
        } else if let Some(p) = state.seat.pointer.take() {
            // Objects created from the old pointer go with it.
            if let Some(r) = state.seat.relative.take() {
                r.destroy();
            }
            if let Some(c) = state.seat.cursor_shape.take() {
                c.destroy();
            }
            if p.version() >= 3 {
                p.release();
            }
            state.seat.pointer_focus = None;
            lost = true;
        }
        if caps.contains(Capability::Keyboard) {
            if state.seat.keyboard.is_none() {
                state.seat.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        } else if let Some(k) = state.seat.keyboard.take() {
            if k.version() >= 3 {
                k.release();
            }
            state.seat.keyboard_focus = None;
            lost = true;
        }
        if lost {
            state.push(WlEvent::SeatInputGone);
        }
    }
}

impl Dispatch<WlPointer, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                state.seat.enter_serial = serial;
                let Some(surf) = state.surf_by_surface(&surface) else {
                    state.seat.pointer_focus = None;
                    return;
                };
                state.seat.pointer_focus = Some(surf);
                state.seat.pointer_pos = (surface_x, surface_y);
                state.push(WlEvent::PointerEnter { surf, x: surface_x, y: surface_y });
            }
            wl_pointer::Event::Leave { surface, .. } => {
                let surf = state.surf_by_surface(&surface).or(state.seat.pointer_focus);
                state.seat.pointer_focus = None;
                if let Some(surf) = surf {
                    state.push(WlEvent::PointerLeave { surf });
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.seat.pointer_pos = (surface_x, surface_y);
                state.push(WlEvent::PointerMotion);
            }
            wl_pointer::Event::Button { button, state: WEnum::Value(bs), .. } => {
                let bs = match bs {
                    wl_pointer::ButtonState::Pressed => ButtonState::Pressed,
                    _ => ButtonState::Released,
                };
                state.push(WlEvent::Button { button, state: bs });
            }
            wl_pointer::Event::AxisSource { axis_source: WEnum::Value(src) } => {
                state.seat.axis.source = Some(src);
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if let Some(i) = axis_index(axis) {
                    state.seat.axis.value[i] += value;
                    state.seat.axis.has_value[i] = true;
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                if let Some(i) = axis_index(axis) {
                    state.seat.axis.value120[i] += value120;
                }
            }
            wl_pointer::Event::AxisStop { axis, .. } => {
                if let Some(i) = axis_index(axis) {
                    state.seat.axis.stop[i] = true;
                }
            }
            wl_pointer::Event::Frame => {
                let frame = std::mem::take(&mut state.seat.axis);
                if !frame.is_empty() {
                    state.push(WlEvent::Axis(frame));
                }
                state.push(WlEvent::PointerFrame);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        } = event
        {
            let utime_us = ((utime_hi as u64) << 32) | utime_lo as u64;
            state.push(WlEvent::RelativeMotion { dx, dy, dx_unaccel, dy_unaccel, utime_us });
        }
    }
}

impl Dispatch<WlKeyboard, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => handle_keymap(state, format, fd, size),
            wl_keyboard::Event::Enter { surface, keys, .. } => {
                let surf = state.surf_by_surface(&surface);
                state.seat.keyboard_focus = surf;
                let keys = keys
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                state.push(WlEvent::KeyboardEnter { surf, keys });
            }
            wl_keyboard::Event::Leave { .. } => {
                let had = state.seat.keyboard_focus.take().is_some();
                if had {
                    state.push(WlEvent::KeyboardLeave);
                }
            }
            wl_keyboard::Event::Key { key, state: WEnum::Value(ks), .. } => {
                // Keys only reach us while one of our surfaces has keyboard focus.
                let pressed = matches!(ks, wl_keyboard::KeyState::Pressed);
                state.push(WlEvent::Key { key, pressed });
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                state.push(WlEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                });
            }
            _ => {}
        }
    }
}

fn handle_keymap(state: &mut WlState, format: WEnum<KeymapFormat>, fd: OwnedFd, size: u32) {
    if format != WEnum::Value(KeymapFormat::XkbV1) {
        state.push(WlEvent::KeymapRejected { reason: format!("unsupported keymap format {format:?}") });
        return;
    }
    match Keymap::from_wayland(fd, size) {
        Ok(km) => {
            if state.keymap.as_ref() != Some(&km) {
                state.keymap = Some(km);
                state.push(WlEvent::KeymapChanged);
            }
        }
        Err(e) => state.push(WlEvent::KeymapRejected { reason: format!("{e:#}") }),
    }
}
