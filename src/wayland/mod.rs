//! Wayland client side: globals, outputs, seat, barrier strips and the grab.
//!
//! Everything here is owned by one task. Protocol callbacks (the `Dispatch` impls) only record
//! state and push [`WlEvent`]s; the controller (the Phase 1 probe, later the portal core) drains
//! them after each dispatch and decides what to do. After [`Wl::connect`] returns, nothing here
//! blocks: reading happens through [`Wl::wait`], writing through [`Wl::flush`].

pub mod grab;
pub mod input;
pub mod outputs;
pub mod strip;

use std::collections::{BTreeMap, VecDeque};
use std::io::ErrorKind;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_output::WlOutput, wl_registry,
    wl_registry::WlRegistry, wl_seat::WlSeat, wl_shm::WlShm, wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1;

use crate::keymap::Keymap;
use crate::watchdog::Watchdog;
pub use input::{AxisFrame, ButtonState};
pub use strip::{Edge, SurfId, SurfKind};

/// Highest wl_seat version we bind. niri offers 9; v9 gives axis_value120 and
/// axis_relative_direction. v10 (key state `repeated`) is not needed.
const SEAT_VERSION: u32 = 9;

/// Things the controller reacts to. Surface-local positions are in logical pixels.
#[derive(Debug)]
pub enum WlEvent {
    /// Output set changed (added, removed, or geometry changed on `wl_output.done`).
    OutputsChanged,
    /// An output global was removed; its layer surfaces are gone or about to be closed.
    OutputRemoved { output: u32 },
    SurfaceConfigured { surf: SurfId, width: u32, height: u32 },
    /// niri sends `closed` only when the surface's output goes away (never recreate there).
    SurfaceClosed { surf: SurfId },
    PointerEnter { surf: SurfId, x: f64, y: f64 },
    PointerLeave { surf: SurfId },
    PointerMotion,
    PointerFrame,
    Button { button: u32, state: ButtonState },
    Axis(AxisFrame),
    /// zwp_relative_pointer_v1: sent while the pointer is on one of our surfaces, locked or not.
    RelativeMotion { dx: f64, dy: f64, dx_unaccel: f64, dy_unaccel: f64, utime_us: u64 },
    KeyboardEnter { surf: Option<SurfId>, keys: Vec<u32> },
    KeyboardLeave,
    Key { key: u32, pressed: bool },
    Modifiers { depressed: u32, latched: u32, locked: u32, group: u32 },
    /// A new (valid) keymap was stored in [`WlState::keymap`].
    KeymapChanged,
    KeymapRejected { reason: String },
    Locked,
    Unlocked,
    InhibitorActive,
    InhibitorInactive,
    /// The seat lost its pointer or keyboard (capability change).
    SeatInputGone,
}

#[derive(Default)]
pub struct Globals {
    pub compositor: Option<WlCompositor>,
    pub shm: Option<WlShm>,
    pub layer_shell: Option<ZwlrLayerShellV1>,
    pub xdg_output_manager: Option<ZxdgOutputManagerV1>,
    pub pointer_constraints: Option<ZwpPointerConstraintsV1>,
    pub relative_pointer_manager: Option<ZwpRelativePointerManagerV1>,
    pub shortcuts_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    pub cursor_shape_manager: Option<WpCursorShapeManagerV1>,
    pub seat: Option<(u32, WlSeat)>,
}

pub struct WlState {
    pub qh: QueueHandle<WlState>,
    pub globals: Globals,
    /// Keyed by registry global name.
    pub outputs: BTreeMap<u32, outputs::Output>,
    pub seat: input::SeatState,
    pub surfaces: BTreeMap<SurfId, strip::LayerSurf>,
    next_surf_id: SurfId,
    pub grab: Option<grab::Grab>,
    pub keymap: Option<Keymap>,
    pub events: VecDeque<WlEvent>,
    pub watchdog: Arc<Watchdog>,
}

pub struct Wl {
    conn: Connection,
    queue: EventQueue<WlState>,
    pub state: WlState,
    afd: AsyncFd<RawFd>,
    need_flush: bool,
}

impl Wl {
    /// Connects and collects globals, outputs and seat capabilities. The two roundtrips here
    /// are the only blocking Wayland calls; they happen before the event loop starts.
    pub fn connect(watchdog: Arc<Watchdog>) -> Result<Self> {
        Self::connect_to(watchdog, None)
    }

    /// Like [`Wl::connect`], but with a display name to use when `WAYLAND_DISPLAY` is unset
    /// (e.g. read from the systemd user environment by a D-Bus-activated service).
    pub fn connect_to(watchdog: Arc<Watchdog>, fallback_display: Option<&str>) -> Result<Self> {
        let conn = match (std::env::var_os("WAYLAND_DISPLAY"), fallback_display) {
            (None, Some(display)) => {
                let path = if display.starts_with('/') {
                    std::path::PathBuf::from(display)
                } else {
                    std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?)
                        .join(display)
                };
                let stream = std::os::unix::net::UnixStream::connect(&path)
                    .with_context(|| format!("connecting to {}", path.display()))?;
                Connection::from_socket(stream).context("Wayland handshake")?
            }
            _ => Connection::connect_to_env().context("connecting to the Wayland display")?,
        };
        watchdog.set_wayland_fd(conn.as_fd().try_clone_to_owned().ok());
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = WlState {
            qh,
            globals: Globals::default(),
            outputs: BTreeMap::new(),
            seat: input::SeatState::default(),
            surfaces: BTreeMap::new(),
            next_surf_id: 1,
            grab: None,
            keymap: None,
            events: VecDeque::new(),
            watchdog,
        };
        queue.roundtrip(&mut state).context("Wayland roundtrip (globals)")?;
        queue.roundtrip(&mut state).context("Wayland roundtrip (outputs, seat)")?;

        let g = &state.globals;
        let missing: Vec<&str> = [
            ("wl_compositor", g.compositor.is_some()),
            ("wl_shm", g.shm.is_some()),
            ("zwlr_layer_shell_v1", g.layer_shell.is_some()),
            ("zxdg_output_manager_v1", g.xdg_output_manager.is_some()),
            ("zwp_pointer_constraints_v1", g.pointer_constraints.is_some()),
            ("zwp_relative_pointer_manager_v1", g.relative_pointer_manager.is_some()),
            ("wl_seat", g.seat.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, ok)| (!ok).then_some(name))
        .collect();
        if !missing.is_empty() {
            bail!("compositor lacks required globals: {}", missing.join(", "));
        }
        if g.shortcuts_inhibit_manager.is_none() {
            tracing::warn!("no zwp_keyboard_shortcuts_inhibit_manager_v1: compositor shortcuts stay local");
        }
        if g.cursor_shape_manager.is_none() {
            tracing::warn!("no wp_cursor_shape_manager_v1: cannot restore the cursor explicitly");
        }

        let afd = AsyncFd::with_interest(
            conn.as_fd().as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
        )?;
        Ok(Self { conn, queue, state, afd, need_flush: false })
    }

    /// Runs the `Dispatch` callbacks for everything already read. Afterwards the controller
    /// drains `state.events`.
    pub fn dispatch_pending(&mut self) -> Result<()> {
        self.queue.dispatch_pending(&mut self.state).context("Wayland dispatch")?;
        Ok(())
    }

    /// Sends queued requests. A full socket is not an error: the next [`Wl::wait`] waits for
    /// writability too.
    pub fn flush(&mut self) -> Result<()> {
        match self.conn.flush() {
            Ok(()) => {
                self.need_flush = false;
                self.flushed();
                Ok(())
            }
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                self.need_flush = true;
                Ok(())
            }
            Err(e) => Err(e).context("Wayland flush"),
        }
    }

    /// Waits until the socket is readable (then reads) or, if a flush is pending, writable
    /// (then flushes). Cancel-safe: dropping the future cancels the prepared read. Returns
    /// immediately if events are already queued.
    pub async fn wait(&mut self) -> Result<()> {
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        if self.need_flush {
            tokio::select! {
                r = self.afd.readable() => {
                    let mut ready = r?;
                    read_guard(guard)?;
                    ready.clear_ready();
                }
                r = self.afd.writable() => {
                    drop(guard);
                    let still_blocked = match self.conn.flush() {
                        Ok(()) => false,
                        Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
                        Err(e) => return Err(e).context("Wayland flush"),
                    };
                    self.need_flush = still_blocked;
                    if still_blocked {
                        r?.clear_ready();
                    } else {
                        self.flushed();
                    }
                }
            }
        } else {
            let mut ready = self.afd.readable().await?;
            read_guard(guard)?;
            ready.clear_ready();
        }
        Ok(())
    }
}

impl Wl {
    /// Everything queued so far is on the socket: if no grab is left, tell the watchdog. Until
    /// then a release that is only queued still counts as a held grab.
    fn flushed(&self) {
        if !self.state.holds_grab() {
            self.state.watchdog.set_grab_held(false);
        }
    }
}

/// wayland-backend reads until the socket would block; "would block" with nothing read just
/// means the readiness was spurious.
fn read_guard(guard: wayland_client::backend::ReadEventsGuard) -> Result<()> {
    match guard.read() {
        Ok(_) => Ok(()),
        Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(e).context("reading Wayland events"),
    }
}

impl WlState {
    pub fn push(&mut self, ev: WlEvent) {
        self.events.push_back(ev);
    }

    fn alloc_surf_id(&mut self) -> SurfId {
        let id = self.next_surf_id;
        self.next_surf_id += 1;
        id
    }

    pub fn surf_by_surface(&self, surface: &WlSurface) -> Option<SurfId> {
        self.surfaces.iter().find(|(_, s)| &s.surface == surface).map(|(id, _)| *id)
    }
}

impl Dispatch<WlRegistry, ()> for WlState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                let g = &mut state.globals;
                match interface.as_str() {
                    "wl_compositor" => g.compositor = Some(registry.bind(name, version.min(4), qh, ())),
                    "wl_shm" => g.shm = Some(registry.bind(name, 1, qh, ())),
                    "zwlr_layer_shell_v1" => {
                        g.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                    "zxdg_output_manager_v1" => {
                        let mgr: ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
                        for (oname, out) in state.outputs.iter_mut() {
                            out.ensure_xdg_output(&mgr, *oname, qh);
                        }
                        state.globals.xdg_output_manager = Some(mgr);
                    }
                    "zwp_pointer_constraints_v1" => {
                        g.pointer_constraints = Some(registry.bind(name, 1, qh, ()));
                    }
                    "zwp_relative_pointer_manager_v1" => {
                        g.relative_pointer_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                        g.shortcuts_inhibit_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    "wp_cursor_shape_manager_v1" => {
                        g.cursor_shape_manager = Some(registry.bind(name, 1, qh, ()));
                    }
                    "wl_seat" if g.seat.is_none() => {
                        let seat: WlSeat = registry.bind(name, version.min(SEAT_VERSION), qh, ());
                        g.seat = Some((name, seat));
                    }
                    "wl_output" => {
                        let wl_output: WlOutput = registry.bind(name, version.min(4), qh, name);
                        let mut out = outputs::Output::new(wl_output);
                        if let Some(mgr) = &state.globals.xdg_output_manager {
                            out.ensure_xdg_output(mgr, name, qh);
                        }
                        state.outputs.insert(name, out);
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(out) = state.outputs.remove(&name) {
                    out.destroy();
                    state.push(WlEvent::OutputRemoved { output: name });
                    state.push(WlEvent::OutputsChanged);
                } else if state.globals.seat.as_ref().is_some_and(|(n, _)| *n == name) {
                    tracing::error!("the wl_seat was removed");
                    state.globals.seat = None;
                    state.seat.drop_devices();
                    state.push(WlEvent::SeatInputGone);
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(WlState: ignore WlCompositor);
delegate_noop!(WlState: ignore WlShm);
delegate_noop!(WlState: ignore WlShmPool);
delegate_noop!(WlState: ignore WlBuffer);
delegate_noop!(WlState: ignore WlSurface);
delegate_noop!(WlState: ignore ZwlrLayerShellV1);
delegate_noop!(WlState: ignore ZxdgOutputManagerV1);
delegate_noop!(WlState: ignore ZwpPointerConstraintsV1);
delegate_noop!(WlState: ignore ZwpRelativePointerManagerV1);
delegate_noop!(WlState: ignore ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(WlState: ignore WpCursorShapeManagerV1);
delegate_noop!(WlState: ignore WpCursorShapeDeviceV1);
