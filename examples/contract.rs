//! Phase 3a contract test: acts as xdg-desktop-portal against a backend started with
//! `serve --fake-zones 2880x1800 --trust-any-caller --dev-control` on a private bus (see
//! dev/contract-test.sh). Checks the rules that keep the frontend and KDE Connect alive.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use zbus::zvariant::{Array, ObjectPath, OwnedObjectPath, OwnedValue, Structure, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy};

const BUS: &str = "org.freedesktop.impl.portal.desktop.layercapture";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.impl.portal.InputCapture";

type Results = HashMap<String, OwnedValue>;

static FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn check(ok: bool, what: &str) {
    println!("{} {what}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        FAILURES.lock().unwrap().push(what.to_owned());
    }
}

fn session_path(n: u32) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/session/1_99/test{n}")).unwrap()
}

fn request_path(n: u32) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("/org/freedesktop/portal/desktop/request/1_99/req{n}")).unwrap()
}

/// Runs a receiver on an EIS fd and records compact event descriptions.
fn eis_client(fd: OwnedFd) -> Arc<Mutex<Vec<String>>> {
    eis_client_opts(fd, false)
}

/// Like [`eis_client`]; with `die_on_start` the client closes its socket as soon as a capture
/// starts (a crashing kdeconnectd).
fn eis_client_opts(fd: OwnedFd, die_on_start: bool) -> Arc<Mutex<Vec<String>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let out = log.clone();
    std::thread::spawn(move || {
        let ctx = ei::Context::new(UnixStream::from(fd)).unwrap();
        let Ok((_c, events)) = ctx.handshake_blocking("contract", ei::handshake::ContextType::Receiver) else {
            out.lock().unwrap().push("handshake-failed".into());
            return;
        };
        for ev in events {
            let s = match ev {
                Ok(EiEvent::SeatAdded(s)) => {
                    s.seat.bind_capabilities(
                        DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll | DeviceCapability::Keyboard,
                    );
                    let _ = ctx.flush();
                    "seat".to_owned()
                }
                Ok(EiEvent::DeviceAdded(d)) => format!("device:{}", d.device.name().unwrap_or_default()),
                Ok(EiEvent::DeviceStartEmulating(e)) => {
                    if die_on_start {
                        out.lock().unwrap().push("died".into());
                        return;
                    }
                    format!("start:{}", e.sequence)
                }
                Ok(EiEvent::DeviceStopEmulating(_)) => "stop".into(),
                Ok(EiEvent::PointerMotion(m)) => format!("motion:{}:{}", m.dx, m.dy),
                Ok(EiEvent::KeyboardKey(k)) => format!("key:{}:{}", k.key, k.state == ei::keyboard::KeyState::Press),
                Ok(EiEvent::Disconnected(_)) | Err(_) => {
                    out.lock().unwrap().push("disconnected".into());
                    return;
                }
                Ok(_) => continue,
            };
            out.lock().unwrap().push(s);
        }
    });
    log
}

async fn wait_for(log: &Arc<Mutex<Vec<String>>>, want: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if log.lock().unwrap().iter().any(|e| e == want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::main]
async fn main() -> zbus::Result<()> {
    let conn = Connection::session().await?;
    let ic = Proxy::new(&conn, BUS, PATH, IFACE).await?;

    // Record every signal the backend emits.
    let signals = Arc::new(Mutex::new(Vec::<(String, String, Results)>::new()));
    {
        let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BUS)?.build();
        let mut stream = MessageStream::for_match_rule(rule, &conn, None).await?;
        let signals = signals.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let hdr = msg.header();
                let member = hdr.member().map(|m| m.to_string()).unwrap_or_default();
                let iface = hdr.interface().map(|i| i.to_string()).unwrap_or_default();
                let sig = msg.body().signature().to_string();
                let opts = msg.body().deserialize::<(OwnedObjectPath, Results)>().map(|(_, o)| o).unwrap_or_default();
                signals.lock().unwrap().push((format!("{iface}.{member}"), sig, opts));
            }
        });
    }

    // Properties.
    let caps: u32 = ic.get_property("SupportedCapabilities").await?;
    check(caps == 3, "SupportedCapabilities == 3");
    let version: u32 = ic.get_property("version").await?;
    check(version == 1, "version == 1");

    // CreateSession.
    let s1 = session_path(1);
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("capabilities", Value::U32(3));
    let (resp, res): (u32, Results) =
        ic.call("CreateSession", &(request_path(1), &s1, "org.example.test", "", &opts)).await?;
    check(resp == 0 && res.get("capabilities").and_then(|v| u32::try_from(v).ok()) == Some(3), "CreateSession → 0 with capabilities 3");

    // GetZones, also for an unknown session.
    let empty: HashMap<&str, Value> = HashMap::new();
    let (resp, res): (u32, Results) = ic.call("GetZones", &(request_path(2), &s1, "org.example.test", &empty)).await?;
    let zones = res.get("zones").and_then(|v| Vec::<(u32, u32, i32, i32)>::try_from(v.clone()).ok());
    let zone_set = res.get("zone_set").and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    check(resp == 0 && zones == Some(vec![(2880, 1800, 0, 0)]) && zone_set >= 1, "GetZones → one zone, zone_set ≥ 1");
    let (resp, res): (u32, Results) =
        ic.call("GetZones", &(request_path(3), session_path(77), "x", &empty)).await?;
    check(resp == 0 && res.contains_key("zones"), "GetZones on an unknown session → 0 (never Closed)");

    // SetPointerBarriers: KDE Connect's types (i, ai), the spec's (u, (iiii)), stale, unknown.
    let kc_barrier = |id: i32| {
        let mut b: HashMap<&str, Value> = HashMap::new();
        b.insert("barrier_id", Value::I32(id));
        b.insert("position", Value::from(Array::from(vec![0i32, 0, 0, 1799])));
        b
    };
    let spec_barrier = |id: u32| {
        let mut b: HashMap<&str, Value> = HashMap::new();
        b.insert("barrier_id", Value::U32(id));
        b.insert("position", Value::from(Structure::from((0i32, 0i32, 0i32, 1799i32))));
        b
    };
    let failed = |res: &Results| res.get("failed_barriers").and_then(|v| Vec::<u32>::try_from(v.clone()).ok());
    let (resp, res): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(4), &s1, "x", &empty, vec![kc_barrier(1)], zone_set))
        .await?;
    check(resp == 0 && failed(&res) == Some(vec![]), "SetPointerBarriers with KDE Connect types (i, ai) accepted");
    let (resp, res): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(5), &s1, "x", &empty, vec![spec_barrier(1)], zone_set))
        .await?;
    check(resp == 0 && failed(&res) == Some(vec![]), "SetPointerBarriers with spec types (u, (iiii)) accepted");
    let (resp, res): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(6), &s1, "x", &empty, vec![spec_barrier(1)], zone_set + 5))
        .await?;
    check(resp == 0 && failed(&res) == Some(vec![1]), "stale zone_set → response 0, all failed");
    let (resp, res): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(7), session_path(77), "x", &empty, vec![kc_barrier(4)], zone_set))
        .await?;
    check(resp == 0 && failed(&res) == Some(vec![4]), "unknown session → response 0, all failed");
    let (_, _): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(8), &s1, "x", &empty, vec![kc_barrier(1)], zone_set))
        .await?;

    // ConnectToEIS + receiver.
    let fd: zbus::zvariant::OwnedFd = ic.call("ConnectToEIS", &(&s1, "x", &empty)).await?;
    let fd: OwnedFd = fd.into();
    let flags = rustix::fs::fcntl_getfl(&fd).unwrap();
    check(flags.contains(rustix::fs::OFlags::NONBLOCK), "ConnectToEIS fd is non-blocking");
    let eis = eis_client(fd);
    check(wait_for(&eis, "device:layercapture pointer", Duration::from_secs(2)).await, "EIS pointer device after bind");

    // Enable + simulated activation.
    let (_, _): (u32, Results) = ic.call("Enable", &(&s1, "x", &empty)).await?;
    let control = Proxy::new(&conn, BUS, "/org/freedesktop/portal/desktop/layercapture", "org.freedesktop.impl.portal.desktop.layercapture.Control").await?;
    let ok: bool = control.call("SimulateActivation", &(&s1,)).await?;
    check(ok, "SimulateActivation");
    check(wait_for(&eis, "start:1", Duration::from_secs(2)).await, "EIS start_emulating sequence 1 (= activation_id)");
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let sigs = signals.lock().unwrap();
        let act = sigs.iter().find(|(n, ..)| n.ends_with(".Activated"));
        check(
            act.is_some_and(|(_, sig, _)| sig == "oa{sv}" || sig == "(oa{sv})"),
            "Activated emitted with signature (oa{sv})",
        );
        if let Some((_, _, o)) = act {
            check(o.get("activation_id").and_then(|v| u32::try_from(v).ok()) == Some(1), "Activated.activation_id == 1 (u)");
            check(o.get("barrier_id").and_then(|v| u32::try_from(v).ok()) == Some(1), "Activated.barrier_id == 1 (u)");
            check(
                o.get("cursor_position").is_some_and(|v| <(f64, f64)>::try_from(v.clone()).is_ok()),
                "Activated.cursor_position is (dd)",
            );
        }
    }

    // Release without activation_id (KDE Connect style): no Deactivated; key released first.
    let mut rel: HashMap<&str, Value> = HashMap::new();
    rel.insert("cursor_position", Value::from((0.0f64, 2400.0f64)));
    let (_, _): (u32, Results) = ic.call("Release", &(&s1, "x", &rel)).await?;
    check(wait_for(&eis, "stop", Duration::from_secs(2)).await, "EIS stop_emulating after Release");
    {
        let log = eis.lock().unwrap();
        let pressed = log.iter().any(|e| e == "key:30:true");
        let rel_i = log.iter().position(|e| e == "key:30:false");
        let stop_i = log.iter().position(|e| e == "stop");
        if pressed {
            check(rel_i.is_some() && rel_i < stop_i, "held key released before stop_emulating");
        } else {
            // --fake-zones has no compositor keymap, hence no keyboard device (unit tests cover it).
            println!("SKIP held-key release (no keyboard device without a compositor keymap)");
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    check(
        !signals.lock().unwrap().iter().any(|(n, ..)| n.ends_with(".Deactivated")),
        "no Deactivated after a client Release",
    );

    // Second activation gets id 2; SetPointerBarriers while active emits Deactivated.
    let ok: bool = control.call("SimulateActivation", &(&s1,)).await?;
    check(ok, "second SimulateActivation (after the pong)");
    check(wait_for(&eis, "start:2", Duration::from_secs(2)).await, "second activation uses sequence 2");
    let (_, _): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(9), &s1, "x", &empty, vec![kc_barrier(1)], zone_set))
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    check(
        signals.lock().unwrap().iter().any(|(n, _, o)| {
            n.ends_with(".Deactivated") && o.get("activation_id").and_then(|v| u32::try_from(v).ok()) == Some(2)
        }),
        "SetPointerBarriers while active → Deactivated(activation_id 2)",
    );

    // ConnectToEIS for an unknown session: a valid fd whose peer is closed.
    let fd: zbus::zvariant::OwnedFd = ic.call("ConnectToEIS", &(session_path(77), "x", &empty)).await?;
    let stream = UnixStream::from(OwnedFd::from(fd));
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 1];
    let eof = matches!(std::io::Read::read(&mut &stream, &mut buf), Ok(0));
    check(eof, "ConnectToEIS for an unknown session → fd with EOF, not an error");

    // A stalled core: handlers still answer (with defaults) instead of failing.
    let _: () = control.call("StallCore", &(3000u32,)).await?;
    let started = Instant::now();
    let r: zbus::Result<(u32, Results)> = ic.call("GetZones", &(request_path(10), &s1, "x", &empty)).await;
    let took = started.elapsed();
    check(r.as_ref().is_ok_and(|(resp, _)| *resp == 0) && took < Duration::from_millis(2800), "GetZones answers 0 within ~2 s while the core is stalled");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // An EIS client that dies during a capture: the backend must end it with Deactivated.
    let s2 = session_path(2);
    let (_, _): (u32, Results) = ic.call("CreateSession", &(request_path(20), &s2, "org.example.test", "", &opts)).await?;
    let (_, res): (u32, Results) = ic.call("GetZones", &(request_path(21), &s2, "x", &empty)).await?;
    let zs = res.get("zone_set").and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    let (_, _): (u32, Results) = ic
        .call("SetPointerBarriers", &(request_path(22), &s2, "x", &empty, vec![kc_barrier(1)], zs))
        .await?;
    let fd2: zbus::zvariant::OwnedFd = ic.call("ConnectToEIS", &(&s2, "x", &empty)).await?;
    let dying = eis_client_opts(fd2.into(), true);
    wait_for(&dying, "device:layercapture pointer", Duration::from_secs(2)).await;
    let (_, _): (u32, Results) = ic.call("Enable", &(&s2, "x", &empty)).await?;
    let ok: bool = control.call("SimulateActivation", &(&s2,)).await?;
    check(ok && wait_for(&dying, "died", Duration::from_secs(2)).await, "second session activated, its client died");
    tokio::time::sleep(Duration::from_millis(800)).await;
    check(
        signals.lock().unwrap().iter().any(|(n, _, o)| {
            n.ends_with(".Deactivated")
                && o.get("activation_id").and_then(|v| u32::try_from(v).ok()) == Some(3)
        }),
        "EIS client death during a capture → Deactivated(activation_id 3)",
    );
    let again: bool = control.call("SimulateActivation", &(&s2,)).await?;
    check(!again, "session with a dead EIS connection stays inert");

    // Session.Close: no Closed signal; the EIS client is disconnected.
    let sess = Proxy::new(&conn, BUS, ObjectPath::try_from(s1.as_str()).unwrap(), "org.freedesktop.impl.portal.Session").await?;
    let _: () = sess.call("Close", &()).await?;
    check(wait_for(&eis, "disconnected", Duration::from_secs(2)).await, "Close disconnects the EIS client");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sigs = signals.lock().unwrap();
    check(!sigs.iter().any(|(n, ..)| n.ends_with(".Closed")), "Session.Closed never emitted");
    check(!sigs.iter().any(|(n, ..)| n.ends_with(".Disabled")), "Disabled never emitted");
    println!("signals seen: {:?}", sigs.iter().map(|(n, s, _)| format!("{n} ({s})")).collect::<Vec<_>>());
    drop(sigs);

    let failures = FAILURES.lock().unwrap();
    if failures.is_empty() {
        println!("ALL CONTRACT CHECKS PASSED");
        Ok(())
    } else {
        println!("{} FAILED: {failures:?}", failures.len());
        std::process::exit(1)
    }
}
