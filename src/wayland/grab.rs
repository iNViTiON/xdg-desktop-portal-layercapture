//! The grab: pointer lock, exclusive keyboard focus and shortcuts inhibitor on a strip, and
//! the one idempotent way to undo all of it.
//!
//! Order of the grab (see the plan's checklist):
//! 1. hide the cursor (`set_cursor(enter_serial, NULL)`);
//! 2. keyboard exclusive + shortcuts inhibitor, commit — before the lock, because niri may soon
//!    require keyboard focus for a lock (upstream PR #4411);
//! 3. `lock_pointer(Oneshot)`: a oneshot lock dies on pointer leave instead of silently
//!    re-locking the next time the pointer touches the strip.
//!
//! The grab is only "active" once both `locked` and `wl_keyboard.enter` on the strip arrived;
//! that decision belongs to the controller.
//!
//! Release: `set_cursor_position_hint` + `wl_surface.commit` + destroy the lock, in that order.
//! niri applies the hint (clamped to the output, not to our 1 px surface) only when the client
//! destroys an active lock; `wp_pointer_warp_v1` is a no-op for layer surfaces in niri.

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{
    self, ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
    zwp_pointer_constraints_v1::Lifetime,
};

use anyhow::{Context, Result, bail};

use super::{SurfId, WlEvent, WlState};

pub struct Grab {
    pub surf: SurfId,
    pub lock: Option<ZwpLockedPointerV1>,
    pub inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    /// `locked` received and no `unlocked` since.
    pub locked: bool,
    pub inhibitor_active: bool,
}

impl WlState {
    /// Starts a grab on `surf`, which must have pointer focus.
    pub fn begin_grab(&mut self, surf: SurfId) -> Result<()> {
        if self.grab.is_some() {
            bail!("a grab is already in progress");
        }
        if self.seat.pointer_focus != Some(surf) {
            bail!("pointer is not on the strip");
        }
        let pointer = self.seat.pointer.clone().context("no pointer")?;
        let seat = self.globals.seat.as_ref().map(|(_, s)| s.clone()).context("no seat")?;
        let constraints = self.globals.pointer_constraints.clone().context("no pointer constraints")?;
        let surface = self.surfaces.get(&surf).context("unknown surface")?.surface.clone();

        // From here on something may be held: tell the watchdog first.
        self.watchdog.set_grab_held(true);
        pointer.set_cursor(self.seat.enter_serial, None, 0, 0);
        self.set_keyboard_exclusive(surf, true);
        let inhibitor = self
            .globals
            .shortcuts_inhibit_manager
            .as_ref()
            .map(|m| m.inhibit_shortcuts(&surface, &seat, &self.qh, ()));
        surface.commit();
        let lock = constraints.lock_pointer(&surface, &pointer, None, Lifetime::Oneshot, &self.qh, ());
        self.grab = Some(Grab {
            surf,
            lock: Some(lock),
            inhibitor,
            locked: false,
            inhibitor_active: false,
        });
        Ok(())
    }

    /// Drops whatever part of the grab exists; safe to call at any time, any number of times.
    /// `target` is a global logical position for the pointer (only applied while the lock is
    /// active, which is the only time niri honours a position hint). Returns whether a position
    /// hint was sent.
    ///
    /// The watchdog keeps treating the grab as held until [`super::Wl::flush`] has actually
    /// put these requests on the socket.
    pub fn release_all(&mut self, target: Option<(f64, f64)>) -> bool {
        let Some(grab) = self.grab.take() else {
            // Belt and braces: nothing should be exclusive without a grab.
            let stray: Vec<SurfId> =
                self.surfaces.iter().filter(|(_, s)| s.keyboard_exclusive).map(|(id, _)| *id).collect();
            for id in stray {
                tracing::error!("surface {id} was keyboard-exclusive without a grab; dropping it");
                self.set_keyboard_exclusive(id, false);
                if let Some(s) = self.surfaces.get(&id) {
                    s.surface.commit();
                }
            }
            return false;
        };
        let surf = self.surfaces.get(&grab.surf);
        let mut hint_sent = false;
        if let (Some(lock), true, Some(target), Some(s)) = (&grab.lock, grab.locked, target, surf) {
            lock.set_cursor_position_hint(target.0 - s.origin.0, target.1 - s.origin.1);
            // The hint is double-buffered: it only reaches niri with a commit, and the commit
            // must come before the destroy.
            s.surface.commit();
            hint_sent = true;
        }
        if let Some(lock) = grab.lock {
            lock.destroy();
        }
        if let Some(inhibitor) = grab.inhibitor {
            inhibitor.destroy();
        }
        if self.surfaces.contains_key(&grab.surf) {
            self.set_keyboard_exclusive(grab.surf, false);
            self.surfaces[&grab.surf].surface.commit();
        }
        // We hid the cursor on the strip; niri only resets it when focus changes. If the hint
        // did not move the pointer off the strip (or there was none), show it again.
        if self.seat.pointer_focus == Some(grab.surf)
            && let Some(dev) = &self.seat.cursor_shape
        {
            dev.set_shape(self.seat.enter_serial, Shape::Default);
        }
        hint_sent
    }

    /// True while anything of a grab exists on our side (requests may still be unflushed).
    pub fn holds_grab(&self) -> bool {
        self.grab.is_some() || self.surfaces.values().any(|s| s.keyboard_exclusive)
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for WlState {
    fn event(
        state: &mut Self,
        lock: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(grab) = state.grab.as_mut().filter(|g| g.lock.as_ref() == Some(lock)) else {
            return;
        };
        match event {
            zwp_locked_pointer_v1::Event::Locked => {
                grab.locked = true;
                state.push(WlEvent::Locked);
            }
            zwp_locked_pointer_v1::Event::Unlocked => {
                grab.locked = false;
                state.push(WlEvent::Unlocked);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for WlState {
    fn event(
        state: &mut Self,
        inhibitor: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(grab) = state.grab.as_mut().filter(|g| g.inhibitor.as_ref() == Some(inhibitor)) else {
            return;
        };
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                grab.inhibitor_active = true;
                state.push(WlEvent::InhibitorActive);
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                grab.inhibitor_active = false;
                state.push(WlEvent::InhibitorInactive);
            }
            _ => {}
        }
    }
}
