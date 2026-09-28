//! Phase 1 prototype: one barrier strip on the left edge, no D-Bus, no EIS.
//!
//! `--no-grab` only logs what reaches the strip (enter/leave, relative motion, the push that
//! would activate). Without it, the first push past the threshold runs a real grab, prints
//! everything captured, and releases after `--auto-release` seconds (at most 8; the watchdog
//! cuts any grab at 10 s regardless), then the probe exits.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::signal::unix::{SignalKind, signal};

use crate::eis::{Mods, Note};
use crate::eisdemo::{ClientKind, EisIo};
use crate::keynames::describe;
use crate::runtime::PidFile;
use crate::watchdog::Watchdog;
use crate::wayland::outputs::Rect;
use crate::wayland::strip::StripSpec;
use crate::wayland::{ButtonState, Edge, SurfId, SurfKind, Wl, WlEvent, WlState};

/// Hard cap for any probe grab, enforced by the watchdog thread.
const HARD_LIMIT: Duration = Duration::from_secs(10);
/// Activating must see both `locked` and keyboard enter within this time.
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(1);
/// After a release, how long to wait for the strip's `leave` before assuming the hint failed.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long to keep observing after the release before exiting.
const AFTER_RELEASE: Duration = Duration::from_secs(3);
/// Release target: this many px inside the zone from the left edge (the strip is x ∈ [0,1)).
const RELEASE_INSET: f64 = 2.0;

#[derive(clap::Args)]
pub struct ProbeArgs {
    /// Only map the strip and log; never lock the pointer or take the keyboard.
    #[arg(long)]
    no_grab: bool,
    /// Output to use (e.g. eDP-1). Default: the first output.
    #[arg(long)]
    output: Option<String>,
    /// Keep the strip this many px away from each corner (niri's hot corner is at (0,0)).
    #[arg(long, default_value_t = 8)]
    corner_margin: i32,
    /// Outward push (px of relative motion) on the strip needed to activate.
    #[arg(long, default_value_t = 24.0, value_parser = positive_f64)]
    pressure: f64,
    /// A motion event only counts as a push if |along-edge| <= max_slope * |outward|, so a
    /// diagonal flick sliding along the edge toward the hot corner does not activate.
    #[arg(long, default_value_t = 0.5, value_parser = non_negative_f64)]
    max_slope: f64,
    /// Seconds until a grab is released automatically.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=8))]
    auto_release: u64,
    /// Freeze the event loop once the grab is active, to test the watchdog.
    #[arg(long)]
    stall: bool,
    /// Don't map the sensor surface that measures where a release lands.
    #[arg(long)]
    no_sensor: bool,
    /// Write the compositor's keymap to this file.
    #[arg(long)]
    dump_keymap: Option<PathBuf>,
    /// Forward the captured input over our EIS server to this receiver (Phase 2 test).
    #[arg(long, value_enum)]
    eis: Option<ClientKind>,
}

fn positive_f64(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v),
        _ => Err(format!("{s:?} is not a positive number")),
    }
}

fn non_negative_f64(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok(v),
        _ => Err(format!("{s:?} is not a non-negative number")),
    }
}

enum Phase {
    Idle,
    Activating { since: Instant, entry: (f64, f64), locked: Option<Duration>, kbd: Option<Duration> },
    Active { since: Instant, entry: (f64, f64) },
    /// `hint_sent`: the release sent a position hint, so the pointer should leave the strip.
    Released { at: Instant, got_leave: bool, warned: bool, hint_sent: bool },
}

struct Probe {
    args: ProbeArgs,
    output: u32,
    strip: SurfId,
    /// Output geometry the strip was built for; the strip is rebuilt when it changes.
    strip_rect: Rect,
    sensor: Option<SurfId>,
    phase: Phase,
    // Pressure tracking for the current contact with the strip.
    on_strip: bool,
    /// Relative motion only counts after the frame that carried the enter, so the motion that
    /// brought the pointer onto the strip is not counted as a push.
    counting: bool,
    pressure: f64,
    fired: bool,
    // Statistics.
    enters: u32,
    would_activate: u32,
    pushes: Vec<f64>,
    // Keyboard tracking during a grab.
    preheld: BTreeSet<u32>,
    pressed: BTreeSet<u32>,
    grabbed_once: bool,
    stray_since: Option<Instant>,
    watchdog_reported: bool,
    /// EIS server + receiver process when `--eis` is given.
    eis: Option<EisIo>,
    eis_child: Option<std::process::Child>,
    /// Activation counter; also the EIS start_emulating sequence (starts at 1).
    activation: u32,
    done: bool,
}

pub async fn run(args: ProbeArgs) -> Result<()> {
    let _pid = PidFile::create()?;
    let watchdog = Watchdog::start(Some(HARD_LIMIT), Duration::from_secs(5));
    let mut wl = Wl::connect(watchdog.clone())?;

    for (name, out) in &wl.state.outputs {
        tracing::info!("output {name}: {:?} logical {:?}", out.name, out.logical);
    }
    let output = match &args.output {
        Some(want) => wl
            .state
            .outputs
            .iter()
            .find(|(_, o)| o.name.as_deref() == Some(want))
            .map(|(n, _)| *n)
            .with_context(|| format!("no output named {want}"))?,
        None => *wl.state.outputs.keys().next().context("no outputs")?,
    };
    if let Some(km) = &wl.state.keymap {
        log_keymap(km, &args.dump_keymap);
    }

    let (strip, strip_rect) = create_strip(&mut wl.state, output, args.corner_margin)?;
    let mut probe = Probe {
        args,
        output,
        strip,
        strip_rect,
        sensor: None,
        phase: Phase::Idle,
        on_strip: false,
        counting: false,
        pressure: 0.0,
        fired: false,
        enters: 0,
        would_activate: 0,
        pushes: Vec::new(),
        preheld: BTreeSet::new(),
        pressed: BTreeSet::new(),
        grabbed_once: false,
        stray_since: None,
        watchdog_reported: false,
        eis: None,
        eis_child: None,
        activation: 0,
        done: false,
    };
    if let Some(kind) = probe.args.eis {
        let (server, client) = crate::eis::socketpair()?;
        probe.eis_child = Some(crate::eisdemo::spawn_receiver(kind, client, None)?);
        let mut io = EisIo::new(server)?;
        if let Some(km) = &wl.state.keymap {
            io.conn.set_keymap(km.clone());
        }
        probe.eis = Some(io);
        tracing::info!("EIS: forwarding captured input to {kind:?}");
    }
    tracing::info!(
        "{}; push left against the strip (threshold {} px). Ctrl+C to stop.",
        if probe.args.no_grab { "NO-GRAB mode" } else { "GRAB mode: first push grabs for a few seconds" },
        probe.args.pressure
    );

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigusr1 = signal(SignalKind::user_defined1())?;
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    enum Wake {
        Wayland,
        Eis(Vec<Note>),
        Tick,
        Stop(&'static str),
        Release,
    }

    let result: Result<()> = async {
        loop {
            wl.dispatch_pending()?;
            while let Some(ev) = wl.state.events.pop_front() {
                probe.handle(&mut wl.state, ev);
            }
            if probe.done {
                break;
            }
            wl.flush()?;
            if let Some(io) = &mut probe.eis {
                io.conn.flush();
            }
            let wake = tokio::select! {
                r = wl.wait() => { r?; Wake::Wayland }
                r = eis_read(&mut probe.eis) => Wake::Eis(r?),
                _ = tick.tick() => Wake::Tick,
                _ = sigint.recv() => Wake::Stop("SIGINT"),
                _ = sigterm.recv() => Wake::Stop("SIGTERM"),
                _ = sigusr1.recv() => Wake::Release,
            };
            match wake {
                Wake::Wayland => {}
                Wake::Eis(notes) => probe.eis_notes(&mut wl.state, notes),
                Wake::Tick => {
                    watchdog.beat();
                    if watchdog.stage1_fired() && !probe.watchdog_reported {
                        probe.watchdog_reported = true;
                        tracing::error!("WATCHDOG stage 1 fired: the Wayland connection was shut down (see watchdog.log)");
                    }
                    probe.on_tick(&mut wl.state);
                }
                Wake::Stop(sig) => {
                    tracing::info!("{sig}: stopping");
                    break;
                }
                Wake::Release => probe.self_release(&mut wl.state, "release command (SIGUSR1)"),
            }
        }
        Ok(())
    }
    .await;

    // Always drop the grab and our surfaces, even after an error.
    wl.state.release_all(None);
    if let Some(io) = &mut probe.eis {
        io.conn.stop_emulating();
        io.conn.disconnect(reis::eis::connection::DisconnectReason::Disconnected, None);
    }
    if let Some(mut child) = probe.eis_child.take() {
        std::thread::sleep(Duration::from_millis(200));
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
    let ids: Vec<SurfId> = wl.state.surfaces.keys().copied().collect();
    for id in ids {
        wl.state.destroy_surface(id);
    }
    let _ = wl.flush();
    probe.summary();
    result
}

/// Waits for the EIS socket (never completes without `--eis`).
async fn eis_read(eis: &mut Option<EisIo>) -> Result<Vec<Note>> {
    match eis {
        Some(io) => io.read().await,
        None => std::future::pending().await,
    }
}

fn create_strip(state: &mut WlState, output: u32, margin: i32) -> Result<(SurfId, Rect)> {
    let rect = state.outputs[&output].logical.context("output has no geometry")?;
    let spec = StripSpec { output, edge: Edge::Left, start: margin, end: rect.height - margin };
    let id = state.create_strip(spec)?;
    tracing::info!(
        "strip: left edge x={} y∈[{}, {}) of output {:?} ({}x{} at {},{})",
        rect.x,
        rect.y + spec.start,
        rect.y + spec.end,
        state.outputs[&output].name,
        rect.width,
        rect.height,
        rect.x,
        rect.y
    );
    Ok((id, rect))
}

fn log_keymap(km: &crate::keymap::Keymap, dump: &Option<PathBuf>) {
    tracing::info!(
        "keymap: {} bytes incl. NUL, first line {:?}",
        km.size(),
        km.first_line()
    );
    if let Some(path) = dump {
        match std::fs::write(path, km.bytes()) {
            Ok(()) => tracing::info!("keymap written to {}", path.display()),
            Err(e) => tracing::warn!("writing keymap to {}: {e}", path.display()),
        }
    }
}

impl Probe {
    fn strip_origin(&self, state: &WlState) -> (f64, f64) {
        state.surfaces.get(&self.strip).map_or((0.0, 0.0), |s| s.origin)
    }

    /// Where the pointer goes on release: the entry height, 2 px inside the zone.
    fn release_target(&self, state: &WlState, entry: (f64, f64)) -> Option<(f64, f64)> {
        let rect = state.outputs.get(&self.output)?.logical?;
        let m = self.args.corner_margin as f64;
        let y = entry.1.clamp(rect.y as f64 + m, (rect.y + rect.height) as f64 - m - 1.0);
        Some((rect.x as f64 + RELEASE_INSET, y))
    }

    fn handle(&mut self, state: &mut WlState, ev: WlEvent) {
        match ev {
            WlEvent::SurfaceConfigured { surf, width, height } => {
                let what = if surf == self.strip { "strip" } else { "sensor" };
                let origin = state.surfaces.get(&surf).map(|s| s.origin);
                tracing::info!("{what} mapped: {width}x{height} at global {origin:?}");
            }
            WlEvent::SurfaceClosed { surf } => {
                if surf == self.strip {
                    tracing::error!("strip closed by the compositor (output going away)");
                    self.force_release(state, "strip closed");
                    self.done = true;
                } else if Some(surf) == self.sensor {
                    self.sensor = None;
                    state.destroy_surface(surf);
                }
            }
            WlEvent::OutputsChanged => self.outputs_changed(state),
            WlEvent::OutputRemoved { output } => {
                tracing::warn!("output {output} removed");
                if output == self.output {
                    self.force_release(state, "output removed");
                    self.done = true;
                }
            }
            WlEvent::PointerEnter { surf, x, y, .. } => self.pointer_enter(state, surf, x, y),
            WlEvent::PointerLeave { surf } => self.pointer_leave(state, surf),
            WlEvent::PointerMotion => {}
            WlEvent::PointerFrame => {
                if self.on_strip {
                    self.counting = true;
                }
            }
            WlEvent::RelativeMotion { dx, dy, dx_unaccel, dy_unaccel, utime_us } => {
                self.relative_motion(state, dx, dy, dx_unaccel, dy_unaccel, utime_us)
            }
            WlEvent::Button { button, state: bs } => {
                if self.capturing() || self.on_strip {
                    tracing::info!("button {} {:?}", describe(button), bs);
                }
                if let (Phase::Active { .. }, Some(io)) = (&self.phase, &mut self.eis) {
                    io.conn.button(button, bs == ButtonState::Pressed);
                }
            }
            WlEvent::Axis(frame) => {
                if self.capturing() || self.on_strip {
                    tracing::info!(
                        "scroll frame: source {:?} value {:?} (has {:?}) value120 {:?} stop {:?}",
                        frame.source,
                        frame.value,
                        frame.has_value,
                        frame.value120,
                        frame.stop
                    );
                }
                if let (Phase::Active { .. }, Some(io)) = (&self.phase, &mut self.eis) {
                    io.conn.axis(&frame);
                }
            }
            WlEvent::KeyboardEnter { surf, keys } => {
                let names: Vec<String> = keys.iter().map(|k| describe(*k)).collect();
                tracing::info!("keyboard enter on {surf:?}; keys already held: {names:?}");
                if surf == Some(self.strip)
                    && let Phase::Activating { since, kbd, .. } = &mut self.phase
                {
                    *kbd = Some(since.elapsed());
                    self.preheld = keys.into_iter().collect();
                    self.check_active(state);
                }
            }
            WlEvent::KeyboardLeave => {
                tracing::info!("keyboard leave");
                if self.capturing() {
                    self.force_release(state, "keyboard focus lost");
                }
            }
            WlEvent::Key { key, pressed } => self.key(key, pressed),
            WlEvent::Modifiers { depressed, latched, locked, group } => {
                tracing::info!(
                    "modifiers: depressed {depressed:#x} latched {latched:#x} locked {locked:#x} group {group}"
                );
                if let Some(io) = &mut self.eis {
                    io.conn.set_modifiers(Mods { depressed, latched, locked, group });
                }
            }
            WlEvent::KeymapChanged => {
                if let Some(km) = &state.keymap {
                    if self.capturing() {
                        tracing::warn!("keymap changed during capture (would be deferred until idle)");
                    }
                    log_keymap(km, &self.args.dump_keymap);
                    if let Some(io) = &mut self.eis {
                        io.conn.set_keymap(km.clone());
                    }
                }
            }
            WlEvent::KeymapRejected { reason } => tracing::warn!("keymap rejected: {reason}"),
            WlEvent::Locked => {
                tracing::info!("pointer locked");
                if let Phase::Activating { since, locked, .. } = &mut self.phase {
                    *locked = Some(since.elapsed());
                    self.check_active(state);
                }
            }
            WlEvent::Unlocked => {
                tracing::info!("pointer unlocked by the compositor");
                if self.capturing() {
                    self.force_release(state, "lock ended by the compositor");
                }
            }
            WlEvent::InhibitorActive => {
                tracing::info!("shortcuts inhibitor ACTIVE: niri should now pass its shortcuts to us")
            }
            WlEvent::InhibitorInactive => {
                tracing::info!("shortcuts inhibitor inactive (Mod+Escape)");
                if self.capturing() {
                    self.self_release(state, "Mod+Escape (inhibitor inactive)");
                }
            }
            WlEvent::SeatInputGone => {
                tracing::warn!("seat lost its pointer or keyboard");
                self.force_release(state, "seat input gone");
            }
        }
    }

    fn eis_notes(&mut self, state: &mut WlState, notes: Vec<Note>) {
        for note in notes {
            match note {
                Note::Disconnected { reason } => {
                    tracing::error!("EIS connection lost: {reason}");
                    if self.capturing() {
                        self.force_release(state, "EIS connection lost");
                    }
                }
                Note::Pong { rtt } => tracing::debug!("EIS pong after {} µs", rtt.as_micros()),
                other => tracing::info!("EIS: {other:?}"),
            }
        }
    }

    fn capturing(&self) -> bool {
        matches!(self.phase, Phase::Activating { .. } | Phase::Active { .. })
    }

    fn outputs_changed(&mut self, state: &mut WlState) {
        for (name, out) in &state.outputs {
            tracing::info!("outputs changed: {name}: {:?} logical {:?}", out.name, out.logical);
        }
        let Some(rect) = state.outputs.get(&self.output).and_then(|o| o.logical) else {
            return;
        };
        if rect != self.strip_rect {
            if self.capturing() {
                self.force_release(state, "output geometry changed");
            }
            tracing::info!("output geometry changed: recreating the strip");
            state.destroy_surface(self.strip);
            if let Some(sensor) = self.sensor.take() {
                state.destroy_surface(sensor);
            }
            match create_strip(state, self.output, self.args.corner_margin) {
                Ok((id, rect)) => {
                    self.strip = id;
                    self.strip_rect = rect;
                }
                Err(e) => {
                    tracing::error!("recreating strip: {e:#}");
                    self.done = true;
                }
            }
            self.on_strip = false;
        }
    }

    fn pointer_enter(&mut self, state: &mut WlState, surf: SurfId, x: f64, y: f64) {
        let Some(s) = state.surfaces.get(&surf) else { return };
        let global = (s.origin.0 + x, s.origin.1 + y);
        match s.kind {
            SurfKind::Strip(_) => {
                self.enters += 1;
                self.on_strip = true;
                self.counting = false;
                self.pressure = 0.0;
                self.fired = false;
                tracing::info!("strip enter #{} at local ({x:.2}, {y:.2}) = global {global:?}", self.enters);
                if let Phase::Released { at, .. } = self.phase {
                    tracing::warn!(
                        "strip re-entered {} ms after release (pointer not moved off the strip?)",
                        at.elapsed().as_millis()
                    );
                }
            }
            SurfKind::Sensor => {
                let since = match self.phase {
                    Phase::Released { at, .. } => format!(" {} ms after release", at.elapsed().as_millis()),
                    _ => String::new(),
                };
                tracing::info!("SENSOR enter at local ({x:.2}, {y:.2}) = global {global:?}{since}");
            }
        }
    }

    fn pointer_leave(&mut self, state: &mut WlState, surf: SurfId) {
        if surf == self.strip {
            tracing::info!("strip leave; outward push during this contact: {:.1} px", self.pressure);
            self.pushes.push(self.pressure);
            self.on_strip = false;
            self.counting = false;
            if let Phase::Released { at, got_leave, hint_sent, .. } = &mut self.phase
                && !*got_leave
            {
                *got_leave = true;
                tracing::info!(
                    "RELEASE CHECK: strip leave {} ms after release ({})",
                    at.elapsed().as_millis(),
                    if *hint_sent { "hint applied" } else { "no hint was sent: pointer moved by you" }
                );
            } else if self.capturing() {
                self.force_release(state, "pointer left the strip");
                if let Phase::Released { got_leave, .. } = &mut self.phase {
                    *got_leave = true;
                }
            }
        } else {
            tracing::info!("sensor leave");
        }
    }

    fn relative_motion(&mut self, state: &mut WlState, dx: f64, dy: f64, dxu: f64, dyu: f64, utime_us: u64) {
        if self.capturing() {
            tracing::info!("rel dx {dx:+.2} dy {dy:+.2} (unaccel {dxu:+.2} {dyu:+.2}) t={utime_us}");
            if let (Phase::Active { .. }, Some(io)) = (&self.phase, &mut self.eis) {
                io.conn.motion(dx, dy);
            }
            return;
        }
        if !self.on_strip || !matches!(self.phase, Phase::Idle) {
            return;
        }
        let edge = Edge::Left;
        let out = edge.outward(dx, dy);
        let along = edge.along(dx, dy);
        let counted = self.counting && out > 0.0 && along.abs() <= self.args.max_slope * out;
        if counted {
            self.pressure += out;
        }
        tracing::info!(
            "on strip: rel dx {dx:+.2} dy {dy:+.2} → {} push {:.1}/{}",
            if counted { "counted" } else if !self.counting { "(entering frame)" } else { "ignored" },
            self.pressure,
            self.args.pressure
        );
        if counted && self.pressure >= self.args.pressure && !self.fired {
            self.fired = true;
            if self.args.no_grab || self.grabbed_once {
                self.would_activate += 1;
                tracing::info!("WOULD ACTIVATE (#{})", self.would_activate);
            } else {
                self.start_grab(state);
            }
        }
    }

    fn start_grab(&mut self, state: &mut WlState) {
        if let Some(io) = &self.eis
            && !io.conn.can_activate()
        {
            tracing::warn!("EIS receiver not ready (no bound pointer, backlog, or ping outstanding): not grabbing");
            self.grabbed_once = false;
            return;
        }
        let (x, y) = state.seat.pointer_pos;
        let origin = self.strip_origin(state);
        let entry = (origin.0 + x, origin.1 + y);
        match state.begin_grab(self.strip) {
            Ok(()) => {
                // The pointer is on the strip now, so it cannot enter the sensor before the
                // release moves it there.
                if let Some(old) = self.sensor.take() {
                    state.destroy_surface(old);
                }
                if !self.args.no_sensor
                    && let Some(rect) = state.outputs.get(&self.output).and_then(|o| o.logical)
                {
                    let m = self.args.corner_margin;
                    match state.create_left_sensor(self.output, 1, 4, m, rect.height - m) {
                        Ok(id) => self.sensor = Some(id),
                        Err(e) => tracing::warn!("sensor: {e:#}"),
                    }
                }
                self.grabbed_once = true;
                self.pressed.clear();
                self.preheld.clear();
                tracing::info!("GRAB started at global {entry:?}; waiting for locked + keyboard enter");
                self.phase = Phase::Activating { since: Instant::now(), entry, locked: None, kbd: None };
            }
            Err(e) => tracing::error!("begin_grab: {e:#}"),
        }
    }

    fn check_active(&mut self, _state: &mut WlState) {
        let Phase::Activating { since, entry, locked: Some(l), kbd: Some(k) } = self.phase else {
            return;
        };
        tracing::info!(
            "ACTIVE: locked after {} ms, keyboard enter after {} ms; auto-release in {} s. \
             Try: move, click, scroll, type, a Mod shortcut (e.g. Mod+O), Mod+Escape.",
            l.as_millis(),
            k.as_millis(),
            self.args.auto_release
        );
        self.phase = Phase::Active { since, entry };
        if let Some(io) = &mut self.eis {
            self.activation += 1;
            tracing::info!("EIS: start_emulating(sequence = activation_id {})", self.activation);
            io.conn.start_emulating(self.activation);
        }
        if self.args.stall {
            tracing::warn!("--stall: freezing the event loop for 40 s; the watchdog must release the grab");
            std::thread::sleep(Duration::from_secs(40));
        }
    }

    fn key(&mut self, key: u32, pressed: bool) {
        if !self.capturing() {
            return;
        }
        if let (Phase::Active { .. }, Some(io)) = (&self.phase, &mut self.eis) {
            io.conn.key(key, pressed);
        }
        if pressed {
            self.pressed.insert(key);
            tracing::info!("key {} pressed", describe(key));
        } else if self.pressed.remove(&key) {
            tracing::info!("key {} released", describe(key));
        } else if self.preheld.remove(&key) {
            tracing::info!("key {} released (held before capture: would not be forwarded)", describe(key));
        } else {
            tracing::info!("key {} released without a press (would be dropped)", describe(key));
        }
    }

    /// Release initiated by us or the user (timer, Mod+Escape, release command): move the
    /// pointer off the strip with the lock's position hint.
    fn self_release(&mut self, state: &mut WlState, reason: &str) {
        let entry = match self.phase {
            Phase::Activating { entry, .. } | Phase::Active { entry, .. } => entry,
            _ => {
                tracing::info!("{reason}: nothing to release");
                state.release_all(None);
                return;
            }
        };
        let target = self.release_target(state, entry);
        if !self.pressed.is_empty() {
            let keys: Vec<String> = self.pressed.iter().map(|k| describe(*k)).collect();
            tracing::info!("would send key releases before stop_emulating: {keys:?}");
        }
        let hint_sent = state.release_all(target);
        if let Some(io) = &mut self.eis
            && io.conn.is_emulating()
        {
            tracing::info!("EIS: releasing held keys/buttons, stop_emulating");
            io.conn.stop_emulating();
        }
        if hint_sent {
            tracing::info!("RELEASED ({reason}); position hint sent: {target:?}");
        } else {
            tracing::info!("RELEASED ({reason}); no position hint (lock not active); pointer stays where it is");
        }
        self.phase = Phase::Released { at: Instant::now(), got_leave: false, warned: false, hint_sent };
    }

    /// Release forced by the compositor (focus loss, unlock, output change): the lock is
    /// already gone or going, so the hint may not apply.
    fn force_release(&mut self, state: &mut WlState, reason: &str) {
        if !self.capturing() {
            state.release_all(None);
            return;
        }
        tracing::warn!("forced release: {reason}");
        self.self_release(state, reason);
    }

    fn on_tick(&mut self, state: &mut WlState) {
        match self.phase {
            Phase::Activating { since, locked, kbd, .. } if since.elapsed() > ACTIVATE_TIMEOUT => {
                tracing::warn!(
                    "ABORT: activation not confirmed within 1 s (locked: {locked:?}, keyboard enter: {kbd:?})"
                );
                state.release_all(None);
                self.phase = Phase::Released { at: Instant::now(), got_leave: false, warned: false, hint_sent: false };
            }
            Phase::Active { since, .. } if since.elapsed() >= Duration::from_secs(self.args.auto_release) => {
                self.self_release(state, "auto-release timer");
            }
            Phase::Released { at, got_leave, ref mut warned, hint_sent } => {
                if !got_leave && !*warned && at.elapsed() > LEAVE_TIMEOUT {
                    *warned = true;
                    if hint_sent {
                        tracing::warn!(
                            "RELEASE CHECK: no strip leave within 1 s: the position hint was NOT applied \
                             (pointer still on the strip; cursor shape restored)"
                        );
                    } else {
                        tracing::info!("no strip leave within 1 s (expected: no hint was sent; cursor shape restored)");
                    }
                }
                if at.elapsed() > AFTER_RELEASE {
                    self.done = true;
                }
            }
            _ => {}
        }

        // A grab object without an activation in progress must not survive.
        if state.grab.is_some() && !self.capturing() {
            let since = *self.stray_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(1) {
                tracing::error!("grab held outside an activation for >1 s: releasing");
                state.release_all(None);
                self.stray_since = None;
            }
        } else {
            self.stray_since = None;
        }
    }

    fn summary(&self) {
        tracing::info!(
            "summary: {} strip contacts, {} would-activate, pushes per contact (px): {:?}",
            self.enters,
            self.would_activate,
            self.pushes.iter().map(|p| (p * 10.0).round() / 10.0).collect::<Vec<_>>()
        );
    }
}
