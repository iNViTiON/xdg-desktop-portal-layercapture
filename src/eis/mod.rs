//! EIS server side for one receiver client (KDE Connect's libei context), built on reis.
//!
//! libei 1.5.0 is strict and KDE Connect never reconnects, so a single protocol violation ends
//! input sharing until kdeconnectd restarts. reis does not enforce the device state machine
//! the way libeis does, so this wrapper does:
//!
//! - devices exist only after `ei_seat.bind`, with capabilities = requested ∩ bound ∩
//!   negotiated;
//! - the keyboard device exists only once a keymap is known (kdeconnectd dereferences the
//!   keymap unconditionally), and there is only one per connection (kdeconnectd closes the
//!   keymap fd it is given, so re-adding devices risks it closing unrelated fds);
//! - `start_emulating` only from resumed, input only while emulating, a frame after every
//!   event group, and releases for every held key/button before `stop_emulating`;
//! - the `start_emulating` sequence is the portal `activation_id` (KDE Connect matches them);
//! - liveness via ping/pong instead of disconnecting on backpressure (a disconnect is final).
//!
//! The type is event-loop agnostic: the owner polls [`EisConn::fd`] for readability and calls
//! [`EisConn::read`], and calls [`EisConn::flush`] after sending.

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use reis::PendingRequestResult;
use reis::enumflags2::BitFlags;
use reis::eis;
use reis::handshake::{EisHandshakeResp, EisHandshaker};
use reis::request::{Connection, Device, DeviceCapability, EisRequest, EisRequestConverter, Seat};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};

use crate::keymap::Keymap;
use crate::wayland::AxisFrame;

/// A socketpair for ConnectToEIS: `(server end, client end)`. Both ends are non-blocking and
/// close-on-exec. libei never sets O_NONBLOCK itself, and with a blocking fd its drain loop can
/// hang kdeconnectd's main thread; libeis hands out non-blocking fds too.
pub fn socketpair() -> Result<(UnixStream, OwnedFd)> {
    use rustix::net::{AddressFamily, SocketFlags, SocketType};
    let (a, b) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .context("socketpair")?;
    Ok((UnixStream::from(a), b))
}

/// Microseconds of CLOCK_MONOTONIC, as ei frame timestamps require.
pub fn now_us() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000 + t.tv_nsec as u64 / 1_000
}

/// What happened while reading from the client.
#[derive(Debug, PartialEq)]
pub enum Note {
    /// Handshake finished with a receiver client.
    Connected { name: Option<String> },
    /// The client bound the seat; devices now exist.
    Bound,
    /// The keyboard device was (re)created with the current keymap.
    KeyboardAdded,
    /// A ping we sent came back.
    Pong { rtt: Duration },
    /// The connection is over (EOF, client disconnect, or a protocol error on either side).
    Disconnected { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mods {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

/// Result of a flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flush {
    Done,
    /// The socket is full; the rest stays buffered. Wait for writability and flush again.
    Blocked,
    Dead,
}

struct Dev {
    dev: Device,
    emulating: bool,
}

struct Ready {
    conv: EisRequestConverter,
    conn: Connection,
    seat: Seat,
    bound: BitFlags<DeviceCapability>,
    pointer: Option<Dev>,
    keyboard: Option<Dev>,
    /// The keymap the keyboard device was created with.
    keyboard_keymap: Option<Keymap>,
}

enum Phase {
    Handshake(EisHandshaker),
    Ready(Box<Ready>),
    Dead,
}

pub struct EisConn {
    ctx: eis::Context,
    phase: Phase,
    /// The compositor's current keymap.
    keymap: Option<Keymap>,
    /// Sequence (= activation_id) while emulating.
    sequence: Option<u32>,
    pressed_keys: BTreeSet<u32>,
    pressed_buttons: BTreeSet<u32>,
    /// Latest compositor modifier state, and what the client last got.
    mods: Option<Mods>,
    sent_mods: Option<Mods>,
    ping: Option<(eis::Pingpong, Instant)>,
    backlog: bool,
    /// Test hook: skip the start_emulating guard (dev-only fault injection).
    fault_double_start: bool,
}

const POINTER_CAPS: [DeviceCapability; 3] =
    [DeviceCapability::Pointer, DeviceCapability::Button, DeviceCapability::Scroll];

fn iface_name(cap: DeviceCapability) -> &'static str {
    match cap {
        DeviceCapability::Pointer => "ei_pointer",
        DeviceCapability::PointerAbsolute => "ei_pointer_absolute",
        DeviceCapability::Keyboard => "ei_keyboard",
        DeviceCapability::Touch => "ei_touchscreen",
        DeviceCapability::Scroll => "ei_scroll",
        DeviceCapability::Button => "ei_button",
        DeviceCapability::Text => "ei_text",
    }
}

impl EisConn {
    /// Takes the server end of [`socketpair`] and sends the handshake.
    pub fn new(server: UnixStream) -> Result<Self> {
        let ctx = eis::Context::new(server).context("creating EIS context")?;
        let hs = EisHandshaker::new(&ctx, 1);
        Ok(Self {
            ctx,
            phase: Phase::Handshake(hs),
            keymap: None,
            sequence: None,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            mods: None,
            sent_mods: None,
            ping: None,
            backlog: false,
            fault_double_start: false,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.ctx.as_fd()
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.phase, Phase::Dead)
    }

    pub fn is_emulating(&self) -> bool {
        self.sequence.is_some()
    }

    /// Ready to start an activation: the client bound a pointer device, nothing is stuck in the
    /// write buffer and no ping is outstanding (a pong proves the client consumed everything
    /// sent before it, so no event of the previous activation can arrive after the next
    /// Activated).
    pub fn can_activate(&self) -> bool {
        let Phase::Ready(r) = &self.phase else { return false };
        r.pointer.is_some() && !self.backlog && self.ping.is_none()
    }

    /// The client bound a pointer device (what a barrier needs to be worth arming).
    pub fn has_pointer(&self) -> bool {
        matches!(&self.phase, Phase::Ready(r) if r.pointer.is_some())
    }

    pub fn has_keyboard(&self) -> bool {
        matches!(&self.phase, Phase::Ready(r) if r.keyboard.is_some())
    }

    pub fn backlog(&self) -> bool {
        self.backlog
    }

    /// How long the current ping has been outstanding.
    pub fn ping_age(&self) -> Option<Duration> {
        self.ping.as_ref().map(|(_, at)| at.elapsed())
    }

    /// Dev-only fault injection: let `start_emulating` run twice, which libei must reject.
    pub fn set_fault_double_start(&mut self, on: bool) {
        self.fault_double_start = on;
    }

    /// Reads and handles everything the client sent. Call when the fd is readable.
    pub fn read(&mut self) -> Vec<Note> {
        let mut notes = Vec::new();
        if self.is_dead() {
            return notes;
        }
        // reis returns Ok(0) when nothing is available and UnexpectedEof at end of stream.
        match self.ctx.read() {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                self.kill(&mut notes, "client closed the connection".into());
                return notes;
            }
            Err(e) => {
                self.kill(&mut notes, format!("read error: {e}"));
                return notes;
            }
        }
        while let Some(pending) = self.ctx.pending_request() {
            if self.is_dead() {
                break;
            }
            match pending {
                PendingRequestResult::Request(req) => self.handle_request(req, &mut notes),
                PendingRequestResult::ParseError(reis::ParseError::InvalidNull) => {
                    // libei sends `ei_handshake.name` with a NULL string when the client never
                    // set a name, and KDE Connect never does. reis rejects the NULL, but the
                    // message is already consumed, so skipping it keeps the stream in sync.
                    tracing::debug!("EIS: skipped a message with a NULL string (unnamed libei client)");
                }
                PendingRequestResult::ParseError(e) => {
                    // The buffer is not drained after a header error; continuing would spin.
                    self.disconnect(eis::connection::DisconnectReason::Protocol, Some("parse error"));
                    notes.push(Note::Disconnected { reason: format!("protocol parse error: {e}") });
                    break;
                }
                PendingRequestResult::InvalidObject(id) => {
                    // Requests for objects we already destroyed are normal races.
                    tracing::debug!("EIS request for unknown object {id}");
                }
            }
        }
        let _ = self.flush();
        notes
    }

    fn handle_request(&mut self, req: eis::Request, notes: &mut Vec<Note>) {
        match &mut self.phase {
            Phase::Dead => {}
            Phase::Handshake(hs) => match hs.handle_request(req) {
                Ok(None) => {}
                Ok(Some(resp)) => self.finish_handshake(resp, notes),
                Err(e) => {
                    self.phase = Phase::Dead;
                    notes.push(Note::Disconnected { reason: format!("handshake failed: {e}") });
                }
            },
            Phase::Ready(r) => {
                if let eis::Request::Pingpong(pp, eis::pingpong::Request::Done { .. }) = &req {
                    if let Some((ours, at)) = &self.ping
                        && ours == pp
                    {
                        notes.push(Note::Pong { rtt: at.elapsed() });
                        self.ping = None;
                    }
                    return;
                }
                if let Err(e) = r.conv.handle_request(req) {
                    self.disconnect(eis::connection::DisconnectReason::Protocol, Some("protocol error"));
                    notes.push(Note::Disconnected { reason: format!("client protocol error: {e}") });
                    return;
                }
                let mut requests = Vec::new();
                while let Some(hl) = r.conv.next_request() {
                    requests.push(hl);
                }
                for hl in requests {
                    match hl {
                        EisRequest::Disconnect => {
                            self.phase = Phase::Dead;
                            self.sequence = None;
                            notes.push(Note::Disconnected { reason: "client disconnected".into() });
                            return;
                        }
                        EisRequest::Bind(bind) => {
                            if let Phase::Ready(r) = &mut self.phase {
                                r.bound = bind.capabilities;
                            }
                            notes.push(Note::Bound);
                            if self.sync_devices() {
                                notes.push(Note::KeyboardAdded);
                            }
                        }
                        other => tracing::debug!("EIS request ignored: {other:?}"),
                    }
                }
            }
        }
    }

    fn finish_handshake(&mut self, resp: EisHandshakeResp, notes: &mut Vec<Note>) {
        let name = resp.name.clone();
        if resp.context_type != eis::handshake::ContextType::Receiver {
            // We only send input; a sender client is a misconfiguration.
            let conv = EisRequestConverter::new(&self.ctx, resp, 1);
            conv.handle().disconnected(
                eis::connection::DisconnectReason::Mode,
                Some("this EIS server only supports receiver contexts"),
            );
            self.phase = Phase::Dead;
            notes.push(Note::Disconnected { reason: "client is not a receiver".into() });
            return;
        }
        let conv = EisRequestConverter::new(&self.ctx, resp, 1);
        let conn = conv.handle().clone();
        // The handshaker queues its replies without flushing.
        let _ = conn.flush();
        let seat = conn.add_seat(
            Some("default"),
            DeviceCapability::Pointer
                | DeviceCapability::Button
                | DeviceCapability::Scroll
                | DeviceCapability::Keyboard,
        );
        self.phase = Phase::Ready(Box::new(Ready {
            conv,
            conn,
            seat,
            bound: BitFlags::empty(),
            pointer: None,
            keyboard: None,
            keyboard_keymap: None,
        }));
        notes.push(Note::Connected { name });
    }

    /// Creates/removes devices to match the bound capabilities and the keymap. Returns true if
    /// a keyboard device was added.
    fn sync_devices(&mut self) -> bool {
        let sequence = self.sequence;
        let keymap = self.keymap.clone();
        let Phase::Ready(r) = &mut self.phase else { return false };
        let negotiated = |cap: DeviceCapability| r.conn.has_interface(iface_name(cap));

        let ptr_caps: BitFlags<DeviceCapability> = POINTER_CAPS
            .into_iter()
            .filter(|c| r.bound.contains(*c) && negotiated(*c))
            .collect();
        let want_pointer = ptr_caps.contains(DeviceCapability::Pointer);
        if !want_pointer && let Some(p) = r.pointer.take() {
            p.dev.remove();
        }
        if want_pointer && r.pointer.is_none() {
            let dev = r.seat.add_device(Some("layercapture pointer"), eis::device::DeviceType::Virtual, ptr_caps, |_| {});
            dev.resumed();
            let mut d = Dev { dev, emulating: false };
            if let Some(seq) = sequence {
                d.dev.start_emulating(seq);
                d.emulating = true;
            }
            r.pointer = Some(d);
        }

        let want_keyboard = r.bound.contains(DeviceCapability::Keyboard) && negotiated(DeviceCapability::Keyboard);
        if !want_keyboard && let Some(k) = r.keyboard.take() {
            k.dev.remove();
            r.keyboard_keymap = None;
        }
        let mut added = false;
        if want_keyboard && r.keyboard.is_none() && let Some(km) = keymap {
            match keymap_memfd(&km) {
                Ok(fd) => {
                    let size = km.size() as u32;
                    let dev = r.seat.add_device(
                        Some("layercapture keyboard"),
                        eis::device::DeviceType::Virtual,
                        DeviceCapability::Keyboard.into(),
                        |d| {
                            if let Some(kbd) = d.interface::<eis::Keyboard>() {
                                // reis dups the fd when queuing it; ours closes afterwards.
                                kbd.keymap(eis::keyboard::KeymapType::Xkb, size, fd.as_fd());
                            }
                        },
                    );
                    dev.resumed();
                    let mut d = Dev { dev, emulating: false };
                    if let Some(seq) = sequence {
                        d.dev.start_emulating(seq);
                        d.emulating = true;
                    }
                    r.keyboard = Some(d);
                    r.keyboard_keymap = Some(km);
                    added = true;
                }
                Err(e) => tracing::error!("creating keymap memfd: {e:#}"),
            }
        }
        let _ = r.conn.flush();
        added
    }

    /// The compositor's keymap changed (or arrived). Creates the keyboard device if the client
    /// bound one. A different keymap replaces the device only while idle; during an activation
    /// it waits until [`EisConn::stop_emulating`].
    pub fn set_keymap(&mut self, km: Keymap) -> bool {
        self.keymap = Some(km);
        self.apply_keymap()
    }

    fn apply_keymap(&mut self) -> bool {
        if self.sequence.is_some() {
            return false;
        }
        if let Phase::Ready(r) = &mut self.phase
            && r.keyboard.is_some()
            && r.keyboard_keymap != self.keymap
        {
            if let Some(k) = r.keyboard.take() {
                k.dev.remove();
            }
            r.keyboard_keymap = None;
        }
        self.sync_devices()
    }

    /// Latest compositor modifier state; forwarded while emulating if it changed.
    pub fn set_modifiers(&mut self, mods: Mods) {
        self.mods = Some(mods);
        self.send_modifiers_if_changed();
    }

    fn send_modifiers_if_changed(&mut self) {
        let Some(mods) = self.mods else { return };
        if self.sent_mods == Some(mods) {
            return;
        }
        let Phase::Ready(r) = &self.phase else { return };
        let Some(k) = r.keyboard.as_ref().filter(|k| k.emulating) else { return };
        let Some(kbd) = k.dev.interface::<eis::Keyboard>() else { return };
        // ei's order is (depressed, locked, latched, group); Wayland's is (depressed, latched,
        // locked, group).
        r.conn.with_next_serial(|serial| kbd.modifiers(serial, mods.depressed, mods.locked, mods.latched, mods.group));
        self.sent_mods = Some(mods);
    }

    /// Begins an activation. `sequence` must be the portal activation_id.
    pub fn start_emulating(&mut self, sequence: u32) {
        let Phase::Ready(r) = &mut self.phase else { return };
        if self.sequence.is_some() && !self.fault_double_start {
            tracing::error!("start_emulating while already emulating; ignored");
            return;
        }
        for d in [r.pointer.as_mut(), r.keyboard.as_mut()].into_iter().flatten() {
            if !d.emulating || self.fault_double_start {
                d.dev.start_emulating(sequence);
                d.emulating = true;
            }
        }
        self.sequence = Some(sequence);
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        self.sent_mods = None;
        self.send_modifiers_if_changed();
    }

    /// Ends the activation: releases held keys and buttons, frames, `stop_emulating`, then a
    /// ping so the next activation can wait until the client consumed all of this.
    pub fn stop_emulating(&mut self) {
        if self.sequence.is_none() {
            return;
        }
        let keys: Vec<u32> = self.pressed_keys.iter().copied().collect();
        for key in keys {
            self.key(key, false);
        }
        let buttons: Vec<u32> = self.pressed_buttons.iter().copied().collect();
        for button in buttons {
            self.button(button, false);
        }
        if let Phase::Ready(r) = &mut self.phase {
            for d in [r.pointer.as_mut(), r.keyboard.as_mut()].into_iter().flatten() {
                if d.emulating {
                    d.dev.stop_emulating();
                    d.emulating = false;
                }
            }
        }
        self.sequence = None;
        self.send_ping();
        // A keymap that changed during the activation takes effect now.
        let _ = self.apply_keymap();
    }

    fn pointer_dev(&self) -> Option<&Device> {
        match &self.phase {
            Phase::Ready(r) => r.pointer.as_ref().filter(|d| d.emulating).map(|d| &d.dev),
            _ => None,
        }
    }

    fn keyboard_dev(&self) -> Option<&Device> {
        match &self.phase {
            Phase::Ready(r) => r.keyboard.as_ref().filter(|d| d.emulating).map(|d| &d.dev),
            _ => None,
        }
    }

    pub fn motion(&mut self, dx: f64, dy: f64) {
        let Some(dev) = self.pointer_dev() else { return };
        let Some(p) = dev.interface::<eis::Pointer>() else { return };
        p.motion_relative(dx as f32, dy as f32);
        dev.frame(now_us());
    }

    pub fn button(&mut self, button: u32, pressed: bool) {
        let changed = if pressed { self.pressed_buttons.insert(button) } else { self.pressed_buttons.remove(&button) };
        if !changed {
            return;
        }
        let Some(dev) = self.pointer_dev() else { return };
        let Some(b) = dev.interface::<eis::Button>() else { return };
        let state = if pressed { eis::button::ButtonState::Press } else { eis::button::ButtonState::Released };
        b.button(button, state);
        dev.frame(now_us());
    }

    /// Forwards one `wl_pointer.frame` worth of scrolling: a wheel as discrete steps only, a
    /// finger/continuous scroll as distance only (never both: KDE Connect would scroll twice),
    /// then any stop.
    pub fn axis(&mut self, f: &AxisFrame) {
        use wayland_client::protocol::wl_pointer::AxisSource;
        let Some(dev) = self.pointer_dev() else { return };
        let Some(s) = dev.interface::<eis::Scroll>() else { return };
        let wheel = matches!(f.source, Some(AxisSource::Wheel) | Some(AxisSource::WheelTilt));
        let mut sent = false;
        if wheel || f.value120 != [0, 0] {
            if f.value120 != [0, 0] {
                s.scroll_discrete(f.value120[0], f.value120[1]);
                sent = true;
            }
        } else if f.has_value[0] || f.has_value[1] {
            s.scroll(f.value[0] as f32, f.value[1] as f32);
            sent = true;
        }
        if f.stop[0] || f.stop[1] {
            s.scroll_stop(f.stop[0] as u32, f.stop[1] as u32, 0);
            sent = true;
        }
        if sent {
            dev.frame(now_us());
        }
    }

    pub fn key(&mut self, key: u32, pressed: bool) {
        let changed = if pressed { self.pressed_keys.insert(key) } else { self.pressed_keys.remove(&key) };
        if !changed {
            // A release without a press (key held before capture) or a repeated press.
            return;
        }
        let Some(dev) = self.keyboard_dev() else { return };
        let Some(k) = dev.interface::<eis::Keyboard>() else { return };
        let state = if pressed { eis::keyboard::KeyState::Press } else { eis::keyboard::KeyState::Released };
        k.key(key, state);
        dev.frame(now_us());
    }

    fn send_ping(&mut self) {
        if self.ping.is_some() {
            return;
        }
        let Phase::Ready(r) = &self.phase else { return };
        let version = r.conn.interface_version("ei_pingpong").unwrap_or(1);
        let pp = r.conn.connection().ping(version);
        self.ping = Some((pp, Instant::now()));
    }

    /// Sends a liveness ping if none is outstanding.
    pub fn ping(&mut self) {
        self.send_ping();
    }

    pub fn flush(&mut self) -> Flush {
        if self.is_dead() {
            return Flush::Dead;
        }
        match self.ctx.flush() {
            Ok(()) => {
                self.backlog = false;
                Flush::Done
            }
            Err(e) if e == rustix::io::Errno::AGAIN => {
                self.backlog = true;
                Flush::Blocked
            }
            Err(e) => {
                tracing::warn!("EIS flush: {e}");
                self.phase = Phase::Dead;
                Flush::Dead
            }
        }
    }

    /// Ends the connection (the client gets `disconnected`, the socket's read side closes).
    pub fn disconnect(&mut self, reason: eis::connection::DisconnectReason, explanation: Option<&str>) {
        if let Phase::Ready(r) = &self.phase {
            r.conn.disconnected(reason, explanation);
        }
        self.phase = Phase::Dead;
        self.sequence = None;
    }

    fn kill(&mut self, notes: &mut Vec<Note>, reason: String) {
        self.phase = Phase::Dead;
        self.sequence = None;
        notes.push(Note::Disconnected { reason });
    }
}

/// A sealed memfd with the keymap text (including the trailing NUL).
fn keymap_memfd(km: &Keymap) -> io::Result<OwnedFd> {
    let fd = memfd_create("layercapture-keymap", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
    let mut off = 0;
    let bytes = km.bytes();
    while off < bytes.len() {
        off += rustix::io::write(&fd, &bytes[off..])?;
    }
    fcntl_add_seals(&fd, SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL)?;
    Ok(fd)
}
