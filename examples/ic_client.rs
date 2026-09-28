//! Phase 3c test client: uses the InputCapture portal through the real xdg-desktop-portal
//! frontend like KDE Connect does (CreateSession, ConnectToEIS, GetZones, one barrier on the
//! left edge, Enable), runs an EIS receiver that prints what it gets, and releases each
//! capture after `RELEASE_AFTER` seconds with a cursor position on the barrier line.
//!
//! Run it in its own app scope so the frontend derives a known app id:
//!   systemd-run --user --scope --unit=app-layercapture.test-$$.scope target/debug/examples/ic_client

use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ashpd::desktop::input_capture::{
    Barrier, Capabilities, ConnectToEISOptions, CreateSessionOptions, EnableOptions, GetZonesOptions, InputCapture,
    ReleaseOptions, SetPointerBarriersOptions,
};
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};

const RELEASE_AFTER: Duration = Duration::from_secs(5);

async fn arm(ic: &InputCapture, session: &ashpd::desktop::Session<InputCapture>) -> ashpd::Result<(i32, i32, i32, i32)> {
    let zones = ic.zones(session, GetZonesOptions::default()).await?.response()?;
    let left = zones.regions().iter().min_by_key(|r| r.x_offset()).expect("at least one zone");
    let (x, y, h) = (left.x_offset(), left.y_offset(), left.height() as i32);
    let barrier = Barrier::new(NonZeroU32::new(1).unwrap(), (x, y, x, y + h - 1));
    let resp = ic
        .set_pointer_barriers(session, &[barrier], zones.zone_set(), SetPointerBarriersOptions::default())
        .await?
        .response()?;
    println!("[client] zone_set {} barrier ({x},{y})-({x},{}) failed: {:?}", zones.zone_set(), y + h - 1, resp.failed_barriers());
    ic.enable(session, EnableOptions::default()).await?;
    println!("[client] enabled: push the pointer against the left edge");
    Ok((x, y, x, y + h - 1))
}

#[tokio::main]
async fn main() -> ashpd::Result<()> {
    let ic = InputCapture::new().await?;
    println!("[client] SupportedCapabilities: {:?}", ic.supported_capabilities().await?);
    let (session, caps) = ic
        .create_session(None, CreateSessionOptions::default().set_capabilities(Capabilities::Keyboard | Capabilities::Pointer))
        .await?;
    println!("[client] session created, capabilities {caps:?}");

    let fd = ic.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
    std::thread::spawn(move || {
        let ctx = ei::Context::new(UnixStream::from(fd)).unwrap();
        let (_c, events) = ctx.handshake_blocking("layercapture-ic-client", ei::handshake::ContextType::Receiver).unwrap();
        for ev in events {
            match ev {
                Ok(EiEvent::SeatAdded(s)) => {
                    s.seat.bind_capabilities(
                        DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll | DeviceCapability::Keyboard,
                    );
                    let _ = ctx.flush();
                }
                Ok(EiEvent::Frame(_)) => {}
                Ok(EiEvent::DeviceStartEmulating(e)) => println!("[eis] start_emulating sequence {}", e.sequence),
                Ok(EiEvent::PointerMotion(m)) => println!("[eis] motion {:+.1} {:+.1}", m.dx, m.dy),
                Ok(EiEvent::KeyboardKey(k)) => println!("[eis] key {} {:?}", k.key, k.state),
                Ok(EiEvent::Disconnected(d)) => {
                    println!("[eis] disconnected: {:?}", d.reason);
                    return;
                }
                Ok(other) => println!("[eis] {}", format!("{other:?}").chars().take(100).collect::<String>()),
                Err(e) => {
                    println!("[eis] error: {e}");
                    return;
                }
            }
        }
    });

    let mut activated = ic.receive_activated().await?;
    let mut deactivated = ic.receive_deactivated().await?;
    let mut zones_changed = ic.receive_zones_changed().await?;
    let mut barrier = arm(&ic, &session).await?;
    let mut pending_release: Option<(u32, tokio::time::Instant)> = None;

    loop {
        let sleep_until = pending_release.map(|(_, t)| t).unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            Some(a) = activated.next() => {
                println!("[client] Activated id {:?} cursor {:?} barrier {:?}", a.activation_id(), a.cursor_position(), a.barrier_id());
                if let Some(id) = a.activation_id() {
                    pending_release = Some((id, tokio::time::Instant::now() + RELEASE_AFTER));
                }
            }
            Some(d) = deactivated.next() => println!("[client] Deactivated id {:?}", d.activation_id()),
            Some(z) = zones_changed.next() => {
                println!("[client] ZonesChanged zone_set {:?}: re-arming", z.zone_set());
                barrier = arm(&ic, &session).await?;
            }
            _ = tokio::time::sleep_until(sleep_until), if pending_release.is_some() => {
                let (id, _) = pending_release.take().unwrap();
                // Like KDE Connect: a position on the barrier line (the backend moves it inside).
                let pos = (barrier.0 as f64, (barrier.1 + barrier.3) as f64 / 2.0);
                println!("[client] Release({id}) at {pos:?}");
                ic.release(&session, ReleaseOptions::default().set_activation_id(id).set_cursor_position(pos)).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[client] closing");
                session.close().await?;
                return Ok(());
            }
        }
    }
}
