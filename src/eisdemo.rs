//! Phase 2 test harness: our EIS server against a real libei receiver, without any grab.
//!
//! `eis-demo` creates the same socketpair ConnectToEIS will hand out, spawns a receiver on the
//! client end (libei's `ei-debug-events --receiver`, or our own `eis-dump`), and plays a
//! scripted sequence of input using the compositor's real keymap. `--fault double-start`
//! breaks the protocol on purpose to check that libei rejects it and that we notice the EOF.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use crate::eis::{self, EisConn, Flush, Mods, Note};
use crate::keymap::Keymap;
use crate::watchdog::Watchdog;
use crate::wayland::{AxisFrame, Wl};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum ClientKind {
    /// libei's `ei-debug-events --receiver` (prints every event).
    DebugEvents,
    /// Our `eis-dump` receiver: checks and saves the keymap, prints events.
    Dump,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Fault {
    /// Send start_emulating twice without stop; libei must disconnect.
    DoubleStart,
}

#[derive(clap::Args)]
pub struct EisDemoArgs {
    /// Receiver to spawn on the client end of the socketpair.
    #[arg(long, value_enum, default_value = "debug-events")]
    client: ClientKind,
    /// With `--client dump`: where to write the keymap the receiver got.
    #[arg(long)]
    keymap_out: Option<PathBuf>,
    /// Break the protocol on purpose.
    #[arg(long, value_enum)]
    fault: Option<Fault>,
}

/// Spawns a receiver on `client` (the client end of [`eis::socketpair`]). The child gets a
/// dup without close-on-exec; our copies are closed so EOF works when the child exits.
pub fn spawn_receiver(kind: ClientKind, client: OwnedFd, keymap_out: Option<&PathBuf>) -> Result<Child> {
    let inherit = rustix::io::dup(&client).context("dup for child")?; // dup() clears CLOEXEC
    let fdnum = inherit.as_raw_fd();
    let mut cmd = match kind {
        ClientKind::DebugEvents => {
            let mut c = Command::new("ei-debug-events");
            c.arg("--receiver").arg(format!("--socketfd={fdnum}"));
            c
        }
        ClientKind::Dump => {
            let mut c = Command::new(std::env::current_exe()?);
            c.arg("eis-dump").arg("--socketfd").arg(fdnum.to_string());
            if let Some(p) = keymap_out {
                c.arg("--keymap-out").arg(p);
            }
            c
        }
    };
    let child = cmd.spawn().context("spawning the EIS receiver")?;
    drop(inherit);
    drop(client);
    Ok(child)
}

/// Waits for the compositor keymap (sent shortly after the keyboard is created).
pub async fn wayland_keymap() -> Result<Keymap> {
    let wd = Watchdog::start(None, Duration::from_secs(5));
    let mut wl = Wl::connect(wd)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        wl.dispatch_pending()?;
        wl.state.events.clear();
        if let Some(km) = &wl.state.keymap {
            return Ok(km.clone());
        }
        wl.flush()?;
        tokio::time::timeout_at(deadline, wl.wait()).await.context("no keymap from the compositor")??;
    }
}

pub struct EisIo {
    pub conn: EisConn,
    afd: AsyncFd<std::os::fd::RawFd>,
}

impl EisIo {
    pub fn new(server: UnixStream) -> Result<Self> {
        let conn = EisConn::new(server)?;
        let afd = AsyncFd::with_interest(conn.fd().as_raw_fd(), Interest::READABLE)?;
        Ok(Self { conn, afd })
    }

    /// Readiness for use in a hand-written poll over many connections. Clears the readiness:
    /// the caller must then [`EisConn::read`], which drains the socket.
    pub fn poll_readable(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        match self.afd.poll_read_ready(cx) {
            std::task::Poll::Ready(Ok(mut guard)) => {
                guard.clear_ready();
                std::task::Poll::Ready(())
            }
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(()),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    /// Waits until readable, then reads.
    pub async fn read(&mut self) -> Result<Vec<Note>> {
        let mut ready = self.afd.readable().await?;
        ready.clear_ready();
        Ok(self.conn.read())
    }

    /// Reads until `pred` holds or the timeout passes; returns all notes seen.
    pub async fn read_until(&mut self, timeout: Duration, mut pred: impl FnMut(&EisConn, &[Note]) -> bool) -> Result<Vec<Note>> {
        let mut notes = self.conn.read();
        let deadline = tokio::time::Instant::now() + timeout;
        while !pred(&self.conn, &notes) {
            match tokio::time::timeout_at(deadline, self.read()).await {
                Ok(r) => notes.extend(r?),
                Err(_) => bail!("timed out; notes so far: {notes:?}"),
            }
        }
        Ok(notes)
    }
}

fn step(label: &str) {
    tracing::info!("server → {label}");
}

pub async fn run(args: EisDemoArgs) -> Result<()> {
    let keymap = wayland_keymap().await?;
    tracing::info!("compositor keymap: {} bytes incl. NUL", keymap.size());

    let (server, client) = eis::socketpair()?;
    let mut child = spawn_receiver(args.client, client, args.keymap_out.as_ref())?;
    let mut io = EisIo::new(server)?;
    io.conn.set_keymap(keymap);

    let notes = io
        .read_until(Duration::from_secs(3), |c, _| c.can_activate() && c.has_keyboard())
        .await
        .context("receiver did not bind pointer + keyboard")?;
    tracing::info!("receiver ready: {notes:?}");

    let pause = || tokio::time::sleep(Duration::from_millis(30));
    let flush = |c: &mut EisConn| {
        let f = c.flush();
        if f != Flush::Done {
            tracing::warn!("flush: {f:?}");
        }
    };

    if args.fault == Some(Fault::DoubleStart) {
        io.conn.set_fault_double_start(true);
        step("start_emulating(1), then start_emulating(2) without stop (protocol violation)");
        io.conn.start_emulating(1);
        io.conn.start_emulating(2);
        flush(&mut io.conn);
        let started = Instant::now();
        match io.read_until(Duration::from_secs(2), |c, _| c.is_dead()).await {
            Ok(notes) => tracing::info!("EIS lost after {} ms: {notes:?} (expected)", started.elapsed().as_millis()),
            Err(e) => tracing::error!("FAULT NOT DETECTED: {e:#}"),
        }
        wait_child(&mut child, Duration::from_secs(2)).await;
        return Ok(());
    }

    step("modifiers (Num Lock locked), start_emulating(1)");
    io.conn.set_modifiers(Mods { depressed: 0, latched: 0, locked: 0x10, group: 0 });
    io.conn.start_emulating(1);
    flush(&mut io.conn);
    pause().await;

    step("motion (+5, -3)");
    io.conn.motion(5.0, -3.0);
    flush(&mut io.conn);
    pause().await;

    for (name, code) in [("left", 0x110), ("right", 0x111), ("middle", 0x112)] {
        step(&format!("button {name} press + release"));
        io.conn.button(code, true);
        io.conn.button(code, false);
        flush(&mut io.conn);
        pause().await;
    }

    step("wheel: one notch down (discrete only)");
    io.conn.axis(&AxisFrame {
        source: Some(wayland_client::protocol::wl_pointer::AxisSource::Wheel),
        value: [0.0, 15.0],
        has_value: [false, true],
        value120: [0, 120],
        stop: [false, false],
    });
    step("finger scroll 3.5 down, then stop");
    io.conn.axis(&AxisFrame {
        source: Some(wayland_client::protocol::wl_pointer::AxisSource::Finger),
        value: [0.0, 3.5],
        has_value: [false, true],
        ..Default::default()
    });
    io.conn.axis(&AxisFrame {
        source: Some(wayland_client::protocol::wl_pointer::AxisSource::Finger),
        stop: [false, true],
        ..Default::default()
    });
    flush(&mut io.conn);
    pause().await;

    step("Shift+a: shift press, a press/release, shift release (with modifiers)");
    io.conn.key(42, true);
    io.conn.set_modifiers(Mods { depressed: 0x1, latched: 0, locked: 0x10, group: 0 });
    io.conn.key(30, true);
    io.conn.key(30, false);
    io.conn.key(42, false);
    io.conn.set_modifiers(Mods { depressed: 0, latched: 0, locked: 0x10, group: 0 });
    flush(&mut io.conn);
    pause().await;

    step("Caps Lock on (locked modifier)");
    io.conn.key(58, true);
    io.conn.set_modifiers(Mods { depressed: 0, latched: 0, locked: 0x12, group: 0 });
    io.conn.key(58, false);
    flush(&mut io.conn);
    pause().await;

    step("key b pressed and still held at stop_emulating (release must be synthesized)");
    io.conn.key(48, true);
    io.conn.stop_emulating();
    flush(&mut io.conn);
    let notes = io.read_until(Duration::from_secs(1), |c, _| c.can_activate()).await?;
    tracing::info!("pong after stop: {notes:?}");

    step("second activation: start_emulating(2), motion (-1, 0), stop");
    io.conn.start_emulating(2);
    io.conn.motion(-1.0, 0.0);
    io.conn.stop_emulating();
    flush(&mut io.conn);
    io.read_until(Duration::from_secs(1), |c, _| c.can_activate()).await?;

    step("disconnect");
    io.conn.disconnect(reis::eis::connection::DisconnectReason::Disconnected, None);
    wait_child(&mut child, Duration::from_secs(2)).await;
    Ok(())
}

async fn wait_child(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!("receiver exited: {status}");
                return;
            }
            Ok(None) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(20)).await,
            _ => {
                tracing::warn!("receiver still running after {} ms; killing it", timeout.as_millis());
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// `eis-dump`: a minimal reis receiver. Binds everything, checks and saves the keymap, prints
/// the events. Used to verify what a libei-style receiver gets from us.
pub fn dump(socketfd: i32, keymap_out: Option<PathBuf>) -> Result<()> {
    use reis::ei;
    use reis::event::{DeviceCapability, EiEvent};
    // SAFETY: the parent passed this fd to us for our exclusive use.
    let fd = unsafe { OwnedFd::from_raw_fd(socketfd) };
    let ctx = ei::Context::new(UnixStream::from(fd))?;
    let (_conn, events) = ctx.handshake_blocking("layercapture-eis-dump", ei::handshake::ContextType::Receiver)?;
    for ev in events {
        let ev = ev?;
        match &ev {
            EiEvent::SeatAdded(s) => {
                s.seat.bind_capabilities(
                    DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll | DeviceCapability::Keyboard,
                );
                ctx.flush()?;
                println!("[dump] seat added, bound pointer+button+scroll+keyboard");
            }
            EiEvent::DeviceAdded(d) => {
                println!("[dump] device added: {:?}", d.device.name());
                if let Some(km) = d.device.keymap() {
                    let mut buf = vec![0u8; km.size as usize];
                    let n = rustix::io::pread(&km.fd, &mut buf, 0)?;
                    let nul = buf.last() == Some(&0);
                    println!(
                        "[dump] keymap: type {:?}, size {}, read {n}, NUL-terminated: {}",
                        km.type_, km.size, if nul { "yes" } else { "NO" }
                    );
                    if km.size == 0 || !nul {
                        println!("[dump] KEYMAP INVALID (kdeconnectd would fail)");
                    }
                    if let Some(path) = &keymap_out {
                        std::fs::write(path, &buf[..buf.len().saturating_sub(1)])?;
                        println!("[dump] keymap text written to {}", path.display());
                    }
                }
            }
            EiEvent::Disconnected(d) => {
                println!("[dump] disconnected: {:?}", d.reason);
                return Ok(());
            }
            EiEvent::Frame(_) => {}
            other => println!("[dump] {}", short(other)),
        }
    }
    Ok(())
}

fn short(ev: &reis::event::EiEvent) -> String {
    use reis::event::EiEvent as E;
    match ev {
        E::DeviceStartEmulating(e) => format!("start_emulating sequence {}", e.sequence),
        E::DeviceStopEmulating(_) => "stop_emulating".into(),
        E::DeviceResumed(_) => "resumed".into(),
        E::PointerMotion(m) => format!("motion {:+} {:+}", m.dx, m.dy),
        E::Button(b) => format!("button {} {:?}", b.button, b.state),
        E::KeyboardKey(k) => format!("key {} {:?}", k.key, k.state),
        E::KeyboardModifiers(m) => format!(
            "modifiers depressed {:#x} latched {:#x} locked {:#x} group {}",
            m.depressed, m.latched, m.locked, m.group
        ),
        E::ScrollDiscrete(s) => format!("scroll_discrete {} {}", s.discrete_dx, s.discrete_dy),
        E::ScrollDelta(s) => format!("scroll {} {}", s.dx, s.dy),
        E::ScrollStop(s) => format!("scroll_stop x={} y={}", s.x, s.y),
        other => format!("{other:?}").chars().take(120).collect(),
    }
}
