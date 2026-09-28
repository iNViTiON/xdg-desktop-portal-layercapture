//! The portal core: one task that owns the Wayland connection, every session and every EIS
//! connection, so activation, forwarding and teardown are single ordered sequences.
//!
//! D-Bus handlers talk to it through [`Cmd`]s and get replies over oneshot channels; the core
//! reports signals through an unbounded channel to the emitter task and never awaits D-Bus.
//!
//! Contract notes (from xdg-desktop-portal 1.20.4 and KDE Connect 26.04 sources):
//! - the frontend forwards Activated only in ENABLED and Deactivated only in ACTIVE, and moves
//!   ACTIVE→ENABLED itself on Release and Enable; a capture that ends on our side must emit
//!   Deactivated or later Activated signals are dropped;
//! - KDE Connect ignores Deactivated/Disabled and re-arms only after ZonesChanged, so we never
//!   emit Disabled and never Session.Closed (kdeconnectd crashes after Closed);
//! - activation_id is also the EIS start_emulating sequence, starting at 1.

use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};

use crate::eis::{Mods, Note};
use crate::eisdemo::EisIo;
use crate::portal::barriers::{self, BarrierSpec, Zone};
use crate::watchdog::Watchdog;
use crate::wayland::outputs::Rect;
use crate::wayland::strip::StripSpec;
use crate::wayland::{ButtonState, SurfId, Wl, WlEvent};

const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(1);
const SELF_RELEASE_COOLDOWN: Duration = Duration::from_secs(3);
const LEAVE_TIMEOUT: Duration = Duration::from_secs(1);
const PING_INTERVAL: Duration = Duration::from_millis(250);
const PING_LIMIT: Duration = Duration::from_millis(750);
const HINT_FAILED_PRESSURE: f64 = 50.0;
const WAYLAND_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Config {
    pub pressure: f64,
    pub max_slope: f64,
    pub corner_margin: i32,
    pub idle_release: Option<Duration>,
    pub max_activation: Option<Duration>,
    pub max_grab: Option<Duration>,
    /// Test mode without Wayland: fixed zones, no strips.
    pub fake_zones: Option<(u32, u32)>,
    /// WAYLAND_DISPLAY from the systemd user environment, if ours was unset.
    pub wayland_display: Option<String>,
}

pub type SessionId = String;

pub struct ZonesReply {
    pub zones: Vec<(u32, u32, i32, i32)>,
    pub zone_set: u32,
}

pub enum Cmd {
    CreateSession { session: SessionId, app_id: String, caps: u32, reply: oneshot::Sender<u32> },
    GetZones { session: SessionId, reply: oneshot::Sender<ZonesReply> },
    SetBarriers {
        session: SessionId,
        barriers: Vec<Result<(u32, [i32; 4]), Option<u32>>>,
        zone_set: u32,
        reply: oneshot::Sender<Vec<u32>>,
    },
    Enable { session: SessionId },
    Disable { session: SessionId },
    Release { session: SessionId, activation_id: Option<u32>, cursor: Option<(f64, f64)> },
    ConnectEis { session: SessionId, server: UnixStream },
    Close { session: SessionId },
    FrontendGone,
    /// Dev only: activate without Wayland (for the isolated contract test).
    SimulateActivation { session: SessionId, reply: oneshot::Sender<bool> },
    /// Dev only: block the core, to test D-Bus timeouts and the watchdog.
    Stall(Duration),
}

#[derive(Debug)]
pub enum Signal {
    Activated { session: SessionId, activation_id: u32, cursor: (f64, f64), barrier_id: u32 },
    Deactivated { session: SessionId, activation_id: u32, cursor: (f64, f64) },
    ZonesChanged { session: SessionId, zone_set: u32 },
}

struct Session {
    app_id: String,
    eis: Option<EisIo>,
    /// The EIS connection ended; KDE Connect never reconnects, so the session stays inert.
    eis_lost: bool,
    barriers: Vec<BarrierSpec>,
    enabled: bool,
    cooldown_until: Option<Instant>,
}

/// The pointer resting on one of our strips.
struct Contact {
    surf: SurfId,
    /// Relative motion counts only after the frame that carried the enter.
    counting: bool,
    pressure: f64,
    fired: bool,
    threshold: f64,
    last_delta: (f64, f64),
}

enum CapPhase {
    Activating { locked: bool, kbd: bool },
    Active { activation_id: u32 },
}

struct Capture {
    session: SessionId,
    surf: Option<SurfId>,
    spec: BarrierSpec,
    rect: Rect,
    entry: (f64, f64),
    cursor: (f64, f64),
    since: Instant,
    phase: CapPhase,
    last_input: Instant,
    last_ping: Instant,
}

/// Why a capture ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum End {
    /// The client asked (Release): no signal (the frontend already moved to ENABLED).
    ClientRelease,
    /// Client call that implies the end without a signal (Enable, Disable, Close).
    ClientSilent,
    /// We ended it (escape, focus loss, timers, EIS trouble, zones): emit Deactivated and
    /// cool down before re-arming, so an escape does not immediately re-grab.
    SelfInitiated,
    /// SetPointerBarriers while active: emit Deactivated, barriers are replaced anyway.
    Suspended,
}

pub struct Core {
    cfg: Config,
    watchdog: Arc<Watchdog>,
    sig_tx: mpsc::UnboundedSender<Signal>,
    wl: Option<Wl>,
    wl_retry_at: Option<Instant>,
    sessions: BTreeMap<SessionId, Session>,
    zones: Vec<Zone>,
    zone_set: u32,
    activation_counter: u32,
    /// Strip surface → (session, barrier id).
    strips: BTreeMap<SurfId, (SessionId, u32)>,
    contact: Option<Contact>,
    cap: Option<Capture>,
    /// After a client Release: when the release happened, until the strip's leave arrives.
    released_at: Option<Instant>,
    mods: Option<Mods>,
    stray_since: Option<Instant>,
    /// Outputs whose layer surfaces niri closed (it is removing them): no zones or strips there
    /// until the global is gone.
    closed_outputs: std::collections::BTreeSet<u32>,
    /// A strip was closed during this dispatch batch.
    closed_pending: bool,
}

fn next_id(counter: &mut u32) -> u32 {
    *counter = counter.wrapping_add(1);
    if *counter == 0 {
        *counter = 1;
    }
    *counter
}

impl Core {
    pub fn new(cfg: Config, watchdog: Arc<Watchdog>, sig_tx: mpsc::UnboundedSender<Signal>) -> Self {
        let mut core = Self {
            cfg,
            watchdog,
            sig_tx,
            wl: None,
            wl_retry_at: None,
            sessions: BTreeMap::new(),
            zones: Vec::new(),
            zone_set: 1,
            activation_counter: 0,
            strips: BTreeMap::new(),
            contact: None,
            cap: None,
            released_at: None,
            mods: None,
            stray_since: None,
            closed_outputs: Default::default(),
            closed_pending: false,
        };
        if let Some((w, h)) = core.cfg.fake_zones {
            core.zones = vec![Zone { output: 0, rect: Rect { x: 0, y: 0, width: w as i32, height: h as i32 } }];
        } else {
            core.try_connect_wayland();
        }
        core
    }

    fn try_connect_wayland(&mut self) {
        if self.wl.is_some() || self.cfg.fake_zones.is_some() {
            return;
        }
        if self.wl_retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        match Wl::connect_to(self.watchdog.clone(), self.cfg.wayland_display.as_deref()) {
            Ok(wl) => {
                tracing::info!("connected to Wayland");
                self.wl = Some(wl);
                self.wl_retry_at = None;
                self.refresh_zones(false);
            }
            Err(e) => {
                tracing::warn!("Wayland not available yet: {e:#}");
                self.wl_retry_at = Some(Instant::now() + WAYLAND_RETRY);
            }
        }
    }

    pub async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) -> Result<()> {
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigusr1 = signal(SignalKind::user_defined1())?;
        let mut tick = tokio::time::interval(Duration::from_millis(250));

        enum Wake {
            Wayland(Result<()>),
            Eis(SessionId),
            Cmd(Option<Cmd>),
            Tick,
            Stop(&'static str),
            Release,
        }

        loop {
            self.dispatch_wayland();
            self.sync_strips();
            self.flush_all();

            let wake = tokio::select! {
                r = wl_wait(&mut self.wl) => Wake::Wayland(r),
                id = eis_ready(&self.sessions) => Wake::Eis(id),
                c = cmds.recv() => Wake::Cmd(c),
                _ = tick.tick() => Wake::Tick,
                _ = sigint.recv() => Wake::Stop("SIGINT"),
                _ = sigterm.recv() => Wake::Stop("SIGTERM"),
                _ = sigusr1.recv() => Wake::Release,
            };
            match wake {
                Wake::Wayland(Ok(())) => {}
                Wake::Wayland(Err(e)) => self.wayland_lost(&format!("{e:#}")),
                Wake::Eis(id) => self.eis_readable(&id),
                Wake::Cmd(Some(cmd)) => self.command(cmd),
                Wake::Cmd(None) => {
                    tracing::info!("D-Bus side gone; stopping");
                    break;
                }
                Wake::Tick => self.tick(),
                Wake::Stop(sig) => {
                    tracing::info!("{sig}: releasing and stopping");
                    break;
                }
                Wake::Release => self.end_capture(End::SelfInitiated, None, "release command (SIGUSR1)"),
            }
        }
        self.end_capture(End::SelfInitiated, None, "shutting down");
        if let Some(wl) = &mut self.wl {
            wl.state.release_all(None);
        }
        self.flush_all();
        Ok(())
    }

    // ---- Wayland ----------------------------------------------------------------------

    fn dispatch_wayland(&mut self) {
        let Some(wl) = &mut self.wl else { return };
        if let Err(e) = wl.dispatch_pending() {
            self.wayland_lost(&format!("{e:#}"));
            return;
        }
        let events: Vec<WlEvent> = wl.state.events.drain(..).collect();
        for ev in events {
            self.wl_event(ev);
        }
        if std::mem::take(&mut self.closed_pending) {
            // niri closes layer surfaces only when their output goes away: zones changed even
            // if the geometry has not been updated yet.
            let zones = self.current_zones();
            if !zones.is_empty() {
                self.zones = zones;
            }
            self.zones_changed();
        }
    }

    fn wayland_lost(&mut self, why: &str) {
        tracing::error!("Wayland connection lost: {why}");
        // The compositor dropped all our objects (and with them any grab).
        if let Some(cap) = self.cap.take()
            && let CapPhase::Active { activation_id } = cap.phase
        {
            if let Some(io) = self.sessions.get_mut(&cap.session).and_then(|s| s.eis.as_mut()) {
                io.conn.stop_emulating();
            }
            let _ = self.sig_tx.send(Signal::Deactivated { session: cap.session, activation_id, cursor: cap.entry });
        }
        self.wl = None;
        self.strips.clear();
        self.contact = None;
        self.watchdog.set_grab_held(false);
        self.watchdog.set_wayland_fd(None);
        self.wl_retry_at = Some(Instant::now() + WAYLAND_RETRY);
        self.zones_changed();
    }

    fn wl_event(&mut self, ev: WlEvent) {
        match ev {
            WlEvent::OutputsChanged => self.refresh_zones(true),
            WlEvent::OutputRemoved { output } => {
                self.closed_outputs.remove(&output);
                self.refresh_zones(true);
            }
            WlEvent::SurfaceClosed { surf } => {
                // niri closes layer surfaces only when their output goes away.
                if let Some((sid, bid)) = self.strips.get(&surf)
                    && let Some(b) = self.sessions.get(sid).and_then(|s| s.barriers.iter().find(|b| b.id == *bid))
                {
                    self.closed_outputs.insert(b.output);
                }
                if let Some(wl) = &mut self.wl {
                    wl.state.destroy_surface(surf);
                }
                self.strips.remove(&surf);
                if self.contact.as_ref().is_some_and(|c| c.surf == surf) {
                    self.contact = None;
                }
                if self.cap.as_ref().and_then(|c| c.surf) == Some(surf) {
                    self.end_capture(End::SelfInitiated, None, "strip closed (output going away)");
                }
                self.closed_pending = true;
            }
            WlEvent::SurfaceConfigured { .. } | WlEvent::PointerMotion => {}
            WlEvent::PointerEnter { surf, .. } => {
                if self.strips.contains_key(&surf) {
                    let threshold = if self.released_at.is_some() { HINT_FAILED_PRESSURE } else { self.cfg.pressure };
                    self.contact = Some(Contact {
                        surf,
                        counting: false,
                        pressure: 0.0,
                        fired: false,
                        threshold,
                        last_delta: (0.0, 0.0),
                    });
                }
            }
            WlEvent::PointerLeave { surf } => {
                if self.contact.as_ref().is_some_and(|c| c.surf == surf) {
                    self.contact = None;
                }
                self.released_at = None;
                if self.cap.as_ref().and_then(|c| c.surf) == Some(surf) {
                    self.end_capture(End::SelfInitiated, None, "pointer left the strip");
                }
            }
            WlEvent::PointerFrame => {
                if let Some(c) = &mut self.contact {
                    c.counting = true;
                }
            }
            WlEvent::RelativeMotion { dx, dy, .. } => self.relative_motion(dx, dy),
            WlEvent::Button { button, state } => {
                if let Some(io) = self.active_eis() {
                    io.conn.button(button, state == ButtonState::Pressed);
                    self.touch_input();
                }
            }
            WlEvent::Axis(frame) => {
                if let Some(io) = self.active_eis() {
                    io.conn.axis(&frame);
                    self.touch_input();
                }
            }
            WlEvent::KeyboardEnter { surf, .. } => {
                let ours = self.cap.as_ref().and_then(|c| c.surf);
                if let Some(cap) = &mut self.cap
                    && let CapPhase::Activating { kbd, .. } = &mut cap.phase
                    && surf.is_some()
                    && surf == ours
                {
                    *kbd = true;
                    self.try_confirm();
                }
            }
            WlEvent::KeyboardLeave => {
                if self.cap.is_some() {
                    self.end_capture(End::SelfInitiated, None, "keyboard focus lost");
                }
            }
            WlEvent::Key { key, pressed } => {
                if let Some(io) = self.active_eis() {
                    io.conn.key(key, pressed);
                    self.touch_input();
                }
            }
            WlEvent::Modifiers { depressed, latched, locked, group } => {
                let mods = Mods { depressed, latched, locked, group };
                self.mods = Some(mods);
                if let Some(io) = self.active_eis() {
                    io.conn.set_modifiers(mods);
                }
            }
            WlEvent::KeymapChanged => {
                let km = self.wl.as_ref().and_then(|w| w.state.keymap.clone());
                if let Some(km) = km {
                    tracing::info!("keymap: {} bytes", km.size());
                    for s in self.sessions.values_mut() {
                        if let Some(io) = &mut s.eis {
                            io.conn.set_keymap(km.clone());
                        }
                    }
                }
            }
            WlEvent::KeymapRejected { reason } => tracing::warn!("keymap rejected: {reason}"),
            WlEvent::Locked => {
                if let Some(cap) = &mut self.cap
                    && let CapPhase::Activating { locked, .. } = &mut cap.phase
                {
                    *locked = true;
                    self.try_confirm();
                }
            }
            WlEvent::Unlocked => {
                if self.cap.is_some() {
                    self.end_capture(End::SelfInitiated, None, "lock ended by the compositor");
                }
            }
            WlEvent::InhibitorActive => tracing::debug!("shortcuts inhibitor active"),
            WlEvent::InhibitorInactive => {
                if self.cap.is_some() {
                    self.end_capture(End::SelfInitiated, None, "Mod+Escape (shortcuts inhibitor inactive)");
                }
            }
            WlEvent::SeatInputGone => {
                if self.cap.is_some() {
                    self.end_capture(End::SelfInitiated, None, "seat lost its pointer or keyboard");
                }
            }
        }
    }

    fn active_eis(&mut self) -> Option<&mut EisIo> {
        let cap = self.cap.as_ref()?;
        if !matches!(cap.phase, CapPhase::Active { .. }) {
            return None;
        }
        self.sessions.get_mut(&cap.session)?.eis.as_mut().filter(|io| !io.conn.is_dead())
    }

    fn touch_input(&mut self) {
        if let Some(cap) = &mut self.cap {
            cap.last_input = Instant::now();
        }
    }

    fn relative_motion(&mut self, dx: f64, dy: f64) {
        if let Some(cap) = &self.cap {
            if matches!(cap.phase, CapPhase::Active { .. })
                && let Some(io) = self.active_eis()
            {
                io.conn.motion(dx, dy);
                self.touch_input();
            }
            return;
        }
        let Some(contact) = &mut self.contact else { return };
        let Some((session, barrier_id)) = self.strips.get(&contact.surf).cloned() else { return };
        let Some(spec) = self.sessions.get(&session).and_then(|s| s.barriers.iter().find(|b| b.id == barrier_id)).copied()
        else {
            return;
        };
        let out = spec.edge.outward(dx, dy);
        let along = spec.edge.along(dx, dy);
        if contact.counting && out > 0.0 && along.abs() <= self.cfg.max_slope * out {
            contact.pressure += out;
            contact.last_delta = (dx, dy);
            if contact.pressure >= contact.threshold && !contact.fired {
                contact.fired = true;
                let surf = contact.surf;
                self.begin_capture(session, surf, spec);
            }
        }
    }

    fn begin_capture(&mut self, session: SessionId, surf: SurfId, spec: BarrierSpec) {
        let Some(s) = self.sessions.get(&session) else { return };
        if !s.eis.as_ref().is_some_and(|io| io.conn.can_activate()) {
            tracing::info!("barrier pushed, but the EIS receiver is not ready (unbound, backlog or ping pending)");
            return;
        }
        let Some(wl) = &mut self.wl else { return };
        let Some(rect) = wl.state.outputs.get(&spec.output).and_then(|o| o.logical) else { return };
        let origin = wl.state.surfaces.get(&surf).map_or((0.0, 0.0), |s| s.origin);
        let (x, y) = wl.state.seat.pointer_pos;
        let entry = (origin.0 + x, origin.1 + y);
        let delta = self.contact.as_ref().map_or((0.0, 0.0), |c| c.last_delta);
        if let Err(e) = wl.state.begin_grab(surf) {
            tracing::warn!("grab failed: {e:#}");
            return;
        }
        tracing::info!("capture starting on barrier {} of session {session}", spec.id);
        let now = Instant::now();
        self.cap = Some(Capture {
            session,
            surf: Some(surf),
            spec,
            rect,
            entry,
            // Where the pointer "would" be: past the barrier by the pushing motion (spec: the
            // position may be outside the zones).
            cursor: (entry.0 + delta.0, entry.1 + delta.1),
            since: now,
            phase: CapPhase::Activating { locked: false, kbd: false },
            last_input: now,
            last_ping: now,
        });
    }

    /// Activation commits only once the lock and the keyboard focus are both confirmed.
    fn try_confirm(&mut self) {
        let Some(cap) = &mut self.cap else { return };
        let CapPhase::Activating { locked: true, kbd: true } = cap.phase else { return };
        let activation_id = next_id(&mut self.activation_counter);
        cap.phase = CapPhase::Active { activation_id };
        cap.last_input = Instant::now();
        let (session, cursor, barrier_id) = (cap.session.clone(), cap.cursor, cap.spec.id);
        tracing::info!("ACTIVATED {activation_id} (session {session}, barrier {barrier_id}, cursor {cursor:?})");
        // Activated first, then start_emulating with the same number (mutter and KWin order).
        let _ = self.sig_tx.send(Signal::Activated { session: session.clone(), activation_id, cursor, barrier_id });
        let mods = self.mods;
        if let Some(io) = self.sessions.get_mut(&session).and_then(|s| s.eis.as_mut()) {
            if let Some(m) = mods {
                io.conn.set_modifiers(m);
            }
            io.conn.start_emulating(activation_id);
        }
    }

    /// Ends the current capture (if any). `requested` is a client Release position.
    fn end_capture(&mut self, how: End, requested: Option<(f64, f64)>, reason: &str) {
        let Some(cap) = self.cap.take() else {
            if let Some(wl) = &mut self.wl {
                wl.state.release_all(None);
            }
            return;
        };
        match cap.phase {
            CapPhase::Activating { locked, kbd } => {
                // Never confirmed: tear down silently, no signal, no EIS traffic, no id used.
                tracing::info!("capture aborted before confirmation ({reason}; locked {locked}, keyboard {kbd})");
                if let Some(wl) = &mut self.wl {
                    wl.state.release_all(None);
                }
            }
            CapPhase::Active { activation_id } => {
                let target = barriers::release_target(&cap.spec, cap.rect, requested, cap.entry);
                if let Some(io) = self.sessions.get_mut(&cap.session).and_then(|s| s.eis.as_mut()) {
                    io.conn.stop_emulating();
                }
                let hint = match &mut self.wl {
                    Some(wl) if cap.surf.is_some() => wl.state.release_all(Some(target)),
                    _ => false,
                };
                tracing::info!(
                    "DEACTIVATED {activation_id} ({reason}); pointer → {target:?}{}",
                    if hint { "" } else { " (no hint sent)" }
                );
                if matches!(how, End::SelfInitiated | End::Suspended) {
                    let _ = self.sig_tx.send(Signal::Deactivated {
                        session: cap.session.clone(),
                        activation_id,
                        cursor: target,
                    });
                }
                if how == End::SelfInitiated {
                    if let Some(s) = self.sessions.get_mut(&cap.session) {
                        s.cooldown_until = Some(Instant::now() + SELF_RELEASE_COOLDOWN);
                    }
                } else if how == End::ClientRelease {
                    self.released_at = Some(Instant::now());
                }
            }
        }
    }

    // ---- zones and strips -------------------------------------------------------------

    fn current_zones(&self) -> Vec<Zone> {
        let Some(wl) = &self.wl else { return self.zones.clone() };
        wl.state
            .outputs
            .iter()
            .filter(|(name, _)| !self.closed_outputs.contains(name))
            .filter_map(|(name, o)| o.logical.map(|rect| Zone { output: *name, rect }))
            .collect()
    }

    /// Re-reads output geometry; if it changed, ends any capture and tells every session.
    fn refresh_zones(&mut self, notify: bool) {
        let zones = self.current_zones();
        if zones.is_empty() || zones == self.zones {
            return;
        }
        tracing::info!("zones: {:?}", zones.iter().map(|z| z.rect).collect::<Vec<_>>());
        let first = self.zones.is_empty();
        self.zones = zones;
        if notify && !first {
            self.zones_changed();
        }
    }

    /// Barriers are only valid for the zone_set they were set for: disarm everything, bump the
    /// zone_set and tell each session (KDE Connect then calls GetZones, SetPointerBarriers,
    /// Enable again).
    fn zones_changed(&mut self) {
        self.end_capture(End::SelfInitiated, None, "zones changed");
        self.zone_set = self.zone_set.wrapping_add(1).max(1);
        for (id, s) in self.sessions.iter_mut() {
            s.barriers.clear();
            s.enabled = false;
            let _ = self.sig_tx.send(Signal::ZonesChanged { session: id.clone(), zone_set: self.zone_set });
        }
    }

    /// Creates and destroys strips so they match the armed barriers.
    fn sync_strips(&mut self) {
        let Some(wl) = &mut self.wl else { return };
        let now = Instant::now();
        let mut want: Vec<(SessionId, BarrierSpec)> = Vec::new();
        for (id, s) in &mut self.sessions {
            if s.cooldown_until.is_some_and(|t| now < t) {
                continue;
            }
            s.cooldown_until = None;
            let ready = s.eis.as_ref().is_some_and(|io| io.conn.has_pointer());
            if s.enabled && !s.eis_lost && ready {
                want.extend(s.barriers.iter().map(|b| (id.clone(), *b)));
            }
        }
        let capture_surf = self.cap.as_ref().and_then(|c| c.surf);
        let stale: Vec<SurfId> = self
            .strips
            .iter()
            .filter(|(surf, (sid, bid))| {
                Some(**surf) != capture_surf && !want.iter().any(|(s, b)| s == sid && b.id == *bid)
            })
            .map(|(surf, _)| *surf)
            .collect();
        for surf in stale {
            wl.state.destroy_surface(surf);
            self.strips.remove(&surf);
            if self.contact.as_ref().is_some_and(|c| c.surf == surf) {
                self.contact = None;
            }
        }
        for (sid, b) in want {
            if self.strips.values().any(|(s, bid)| *s == sid && *bid == b.id) {
                continue;
            }
            let spec = StripSpec { output: b.output, edge: b.edge, start: b.start, end: b.end };
            match wl.state.create_strip(spec) {
                Ok(surf) => {
                    self.strips.insert(surf, (sid, b.id));
                }
                Err(e) => tracing::warn!("creating strip for barrier {}: {e:#}", b.id),
            }
        }
    }

    fn flush_all(&mut self) {
        if let Some(wl) = &mut self.wl
            && let Err(e) = wl.flush()
        {
            self.wayland_lost(&format!("{e:#}"));
        }
        let mut dead = Vec::new();
        for (id, s) in self.sessions.iter_mut() {
            if let Some(io) = &mut s.eis
                && (io.conn.flush() == crate::eis::Flush::Dead || io.conn.is_dead())
            {
                dead.push(id.clone());
            }
        }
        for id in dead {
            self.eis_lost(&id, "write to the EIS client failed");
        }
    }

    // ---- EIS --------------------------------------------------------------------------

    fn eis_readable(&mut self, id: &SessionId) {
        let Some(s) = self.sessions.get_mut(id) else { return };
        let Some(io) = &mut s.eis else { return };
        let notes = io.conn.read();
        let dead = io.conn.is_dead();
        let mut reported = false;
        for note in notes {
            match note {
                Note::Disconnected { reason } => {
                    reported = true;
                    self.eis_lost(id, &reason);
                }
                Note::Pong { .. } => {}
                other => tracing::info!("session {id}: EIS {other:?}"),
            }
        }
        // A dead connection must leave the poll set, or its closed fd stays ready forever.
        if dead && !reported {
            self.eis_lost(id, "EIS connection is dead");
        }
    }

    fn eis_lost(&mut self, id: &SessionId, reason: &str) {
        if self.sessions.get(id).is_none_or(|s| s.eis.is_none()) {
            return;
        }
        if self.cap.as_ref().is_some_and(|c| &c.session == id) {
            self.end_capture(End::SelfInitiated, None, "EIS connection lost");
        }
        if let Some(s) = self.sessions.get_mut(id) {
            s.eis = None;
            s.eis_lost = true;
        }
        tracing::error!(
            "session {id}: EIS connection lost ({reason}). KDE Connect does not reconnect: input sharing \
             stays off until the phone reconnects or `systemctl --user restart \
             app-org.kde.kdeconnect.daemon@autostart.service`"
        );
    }

    // ---- commands ---------------------------------------------------------------------

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::CreateSession { session, app_id, caps, reply } => {
                tracing::info!("session {session} created for {app_id:?} (capabilities {caps})");
                self.sessions.insert(
                    session,
                    Session {
                        app_id,
                        eis: None,
                        eis_lost: false,
                        barriers: Vec::new(),
                        enabled: false,
                        cooldown_until: None,
                    },
                );
                let _ = reply.send(caps & 3);
                if self.wl.is_none() {
                    self.try_connect_wayland();
                }
            }
            Cmd::GetZones { session, reply } => {
                if self.wl.is_none() {
                    self.try_connect_wayland();
                }
                self.refresh_zones(true);
                if !self.sessions.contains_key(&session) {
                    tracing::warn!("GetZones for unknown session {session}");
                }
                let _ = reply.send(self.zones_reply());
            }
            Cmd::SetBarriers { session, barriers, zone_set, reply } => {
                let failed = self.set_barriers(&session, barriers, zone_set);
                let _ = reply.send(failed);
            }
            Cmd::Enable { session } => {
                if self.cap.as_ref().is_some_and(|c| c.session == session) {
                    self.end_capture(End::ClientSilent, None, "Enable while active");
                }
                if let Some(s) = self.sessions.get_mut(&session) {
                    s.enabled = true;
                    tracing::info!("session {session} enabled ({} barriers)", s.barriers.len());
                }
            }
            Cmd::Disable { session } => {
                if self.cap.as_ref().is_some_and(|c| c.session == session) {
                    self.end_capture(End::ClientSilent, None, "Disable");
                }
                if let Some(s) = self.sessions.get_mut(&session) {
                    s.enabled = false;
                    tracing::info!("session {session} disabled");
                }
            }
            Cmd::Release { session, activation_id, cursor } => {
                let current = match &self.cap {
                    Some(Capture { session: s, phase: CapPhase::Active { activation_id: id }, .. }) if *s == session => {
                        Some(*id)
                    }
                    _ => None,
                };
                match (current, activation_id) {
                    (Some(cur), Some(req)) if cur != req => {
                        tracing::info!("Release for stale activation {req} (current {cur}) ignored")
                    }
                    (Some(_), _) => self.end_capture(End::ClientRelease, cursor, "client Release"),
                    // Usually a late Release (the phone's message arrived after the capture ended).
                    (None, _) => tracing::info!("Release while no capture is active in session {session}: ignored"),
                }
            }
            Cmd::ConnectEis { session, server } => {
                let keymap = self.wl.as_ref().and_then(|w| w.state.keymap.clone());
                match self.sessions.get_mut(&session) {
                    Some(s) if s.eis.is_none() && !s.eis_lost => match EisIo::new(server) {
                        Ok(mut io) => {
                            if let Some(km) = keymap {
                                io.conn.set_keymap(km);
                            }
                            s.eis = Some(io);
                            tracing::info!("session {session}: EIS connection set up");
                        }
                        Err(e) => tracing::error!("session {session}: EIS setup failed: {e:#}"),
                    },
                    // Dropping the server end gives the client an immediate EOF.
                    _ => tracing::warn!("ConnectToEIS for unknown or already connected session {session}"),
                }
            }
            Cmd::Close { session } => {
                if self.cap.as_ref().is_some_and(|c| c.session == session) {
                    self.end_capture(End::ClientSilent, None, "session closed");
                }
                if let Some(mut s) = self.sessions.remove(&session) {
                    if let Some(io) = &mut s.eis {
                        io.conn.stop_emulating();
                        io.conn.disconnect(reis::eis::connection::DisconnectReason::Disconnected, None);
                    }
                    tracing::info!("session {session} ({:?}) closed", s.app_id);
                }
            }
            Cmd::FrontendGone => {
                tracing::warn!("xdg-desktop-portal went away: closing all sessions");
                let ids: Vec<SessionId> = self.sessions.keys().cloned().collect();
                for session in ids {
                    self.command(Cmd::Close { session });
                }
            }
            Cmd::SimulateActivation { session, reply } => {
                let ok = self.simulate_activation(session);
                let _ = reply.send(ok);
            }
            Cmd::Stall(d) => {
                tracing::warn!("dev: stalling the core for {} ms", d.as_millis());
                std::thread::sleep(d);
            }
        }
    }

    fn zones_reply(&self) -> ZonesReply {
        let zones = if self.zones.is_empty() {
            // Never empty: KDE Connect calls front() on the list unconditionally.
            vec![(1920, 1080, 0, 0)]
        } else {
            self.zones.iter().map(|z| (z.rect.width as u32, z.rect.height as u32, z.rect.x, z.rect.y)).collect()
        };
        ZonesReply { zones, zone_set: self.zone_set }
    }

    fn set_barriers(
        &mut self,
        session: &SessionId,
        barriers: Vec<Result<(u32, [i32; 4]), Option<u32>>>,
        zone_set: u32,
    ) -> Vec<u32> {
        let all_ids: Vec<u32> = barriers
            .iter()
            .filter_map(|b| match b {
                Ok((id, _)) => Some(*id),
                Err(id) => *id,
            })
            .collect();
        if !self.sessions.contains_key(session) {
            tracing::warn!("SetPointerBarriers for unknown session {session}");
            return all_ids;
        }
        if self.cap.as_ref().is_some_and(|c| &c.session == session) {
            self.end_capture(End::Suspended, None, "SetPointerBarriers while active");
        }
        let stale = zone_set != self.zone_set;
        let mut failed = Vec::new();
        let mut specs = Vec::new();
        for b in barriers {
            match b {
                Ok((id, pos)) if !stale => match barriers::validate(id, pos, &self.zones, self.cfg.corner_margin) {
                    Some(spec) => specs.push(spec),
                    None => failed.push(id),
                },
                Ok((id, _)) => failed.push(id),
                Err(Some(id)) => failed.push(id),
                Err(None) => {}
            }
        }
        tracing::info!(
            "session {session}: {} barrier(s) set, failed {failed:?}{}",
            specs.len(),
            if stale { format!(" (stale zone_set {zone_set}, current {})", self.zone_set) } else { String::new() }
        );
        if let Some(s) = self.sessions.get_mut(session) {
            s.barriers = specs;
            // Suspended until the next Enable (spec).
            s.enabled = false;
        }
        failed
    }

    fn simulate_activation(&mut self, session: SessionId) -> bool {
        let Some(s) = self.sessions.get(&session) else { return false };
        if self.cap.is_some() || !s.eis.as_ref().is_some_and(|io| io.conn.can_activate()) {
            return false;
        }
        let Some(spec) = s.barriers.first().copied() else { return false };
        let rect = self.zones.iter().find(|z| z.output == spec.output).map_or(Rect { x: 0, y: 0, width: 1, height: 1 }, |z| z.rect);
        let now = Instant::now();
        let entry = barriers::strip_origin(&spec, rect);
        self.cap = Some(Capture {
            session,
            surf: None,
            spec,
            rect,
            entry,
            cursor: (entry.0 - 10.0, entry.1),
            since: now,
            phase: CapPhase::Activating { locked: true, kbd: true },
            last_input: now,
            last_ping: now,
        });
        self.try_confirm();
        if let Some(io) = self.active_eis() {
            io.conn.motion(-5.0, 1.0);
            io.conn.key(30, true);
        }
        true
    }

    // ---- timers -----------------------------------------------------------------------

    fn tick(&mut self) {
        self.watchdog.beat();
        let now = Instant::now();

        if self.wl.is_none() && self.wl_retry_at.is_some_and(|t| now >= t) && !self.sessions.is_empty() {
            self.try_connect_wayland();
            if self.wl.is_some() {
                self.zones_changed();
            }
        }

        if let Some(cap) = &self.cap {
            let age = now - cap.since;
            match cap.phase {
                CapPhase::Activating { .. } if age > ACTIVATE_TIMEOUT => {
                    self.end_capture(End::SelfInitiated, None, "lock/keyboard focus not confirmed within 1 s");
                }
                CapPhase::Activating { .. } => {}
                CapPhase::Active { .. } => {
                    let idle = now - cap.last_input;
                    if self.cfg.max_grab.is_some_and(|m| age >= m) {
                        self.end_capture(End::SelfInitiated, None, "--max-grab reached");
                    } else if self.cfg.max_activation.is_some_and(|m| age >= m) {
                        self.end_capture(End::SelfInitiated, None, "--max-activation reached");
                    } else if self.cfg.idle_release.is_some_and(|m| idle >= m) {
                        self.end_capture(End::SelfInitiated, None, "idle release");
                    } else {
                        self.check_liveness(now);
                    }
                }
            }
        }

        if let Some(at) = self.released_at
            && now - at > LEAVE_TIMEOUT
        {
            // The hint did not move the pointer off the strip: re-arm only with a firm push.
            tracing::warn!("no strip leave within 1 s after a release; next activation needs {HINT_FAILED_PRESSURE} px");
            if let Some(c) = &mut self.contact {
                c.fired = false;
                c.pressure = 0.0;
                c.threshold = HINT_FAILED_PRESSURE;
            }
            self.released_at = None;
        }

        // A grab object without a capture must never survive.
        let holds = self.wl.as_ref().is_some_and(|w| w.state.holds_grab());
        if holds && self.cap.is_none() {
            let since = *self.stray_since.get_or_insert(now);
            if now - since > Duration::from_secs(1) {
                tracing::error!("grab held without a capture for >1 s: releasing");
                if let Some(wl) = &mut self.wl {
                    wl.state.release_all(None);
                }
                self.stray_since = None;
            }
        } else {
            self.stray_since = None;
        }
    }

    fn check_liveness(&mut self, now: Instant) {
        let Some(cap) = &mut self.cap else { return };
        let session = cap.session.clone();
        let due = now - cap.last_ping >= PING_INTERVAL;
        if due {
            cap.last_ping = now;
        }
        let Some(io) = self.sessions.get_mut(&session).and_then(|s| s.eis.as_mut()).filter(|io| !io.conn.is_dead())
        else {
            self.end_capture(End::SelfInitiated, None, "EIS connection gone");
            return;
        };
        if io.conn.backlog() || io.conn.ping_age().is_some_and(|a| a > PING_LIMIT) {
            self.end_capture(End::SelfInitiated, None, "EIS receiver is not keeping up");
            return;
        }
        if due {
            io.conn.ping();
        }
    }
}

async fn wl_wait(wl: &mut Option<Wl>) -> Result<()> {
    match wl {
        Some(wl) => wl.wait().await,
        None => std::future::pending().await,
    }
}

/// Resolves with the id of a session whose EIS socket became readable.
async fn eis_ready(sessions: &BTreeMap<SessionId, Session>) -> SessionId {
    std::future::poll_fn(|cx| {
        for (id, s) in sessions {
            if let Some(io) = &s.eis
                && io.poll_readable(cx).is_ready()
            {
                return Poll::Ready(id.clone());
            }
        }
        Poll::Pending
    })
    .await
}
