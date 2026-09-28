//! In-process tests: our server on one end of a socketpair, a reis receiver client on the
//! other. A reis client does not enforce libei's state machine, so these tests assert the
//! exact event sequence the client receives instead of relying on the client to reject it.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use reis::ei;
use reis::event::{DeviceCapability, EiEvent};

use super::*;
use crate::keymap::Keymap;

#[derive(Debug, PartialEq)]
enum Ev {
    SeatAdded,
    DeviceAdded { name: String, keymap_size: Option<u32>, keymap_nul: bool },
    Start(u32),
    Stop,
    Motion(f32, f32),
    Button(u32, bool),
    Key(u32, bool),
    Mods { depressed: u32, latched: u32, locked: u32 },
    Discrete(i32, i32),
    Delta(f32, f32),
    ScrollStop(bool, bool),
    Frame,
    Disconnected,
}

fn keymap() -> Keymap {
    let fd = rustix::fs::memfd_create("t", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let text = b"xkb_keymap { dummy };\n\0";
    rustix::io::write(&fd, text).unwrap();
    Keymap::from_wayland(fd, text.len() as u32).unwrap()
}

/// Runs a receiver client in a thread; it binds everything offered and reports events.
fn spawn_client(fd: OwnedFd) -> mpsc::Receiver<Ev> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let ctx = ei::Context::new(UnixStream::from(fd)).unwrap();
        let Ok((_conn, events)) = ctx.handshake_blocking("test-client", ei::handshake::ContextType::Receiver) else {
            let _ = tx.send(Ev::Disconnected);
            return;
        };
        for ev in events {
            let Ok(ev) = ev else {
                let _ = tx.send(Ev::Disconnected);
                return;
            };
            let out = match ev {
                EiEvent::SeatAdded(s) => {
                    s.seat.bind_capabilities(
                        DeviceCapability::Pointer
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll
                            | DeviceCapability::Keyboard,
                    );
                    let _ = ctx.flush();
                    Ev::SeatAdded
                }
                EiEvent::DeviceAdded(d) => {
                    let km = d.device.keymap();
                    let keymap_nul = km.is_some_and(|k| {
                        let mut buf = vec![0u8; k.size as usize];
                        rustix::io::pread(&k.fd, &mut buf, 0).is_ok() && buf.last() == Some(&0)
                    });
                    Ev::DeviceAdded {
                        name: d.device.name().unwrap_or_default().to_owned(),
                        keymap_size: km.map(|k| k.size),
                        keymap_nul,
                    }
                }
                EiEvent::DeviceStartEmulating(e) => Ev::Start(e.sequence),
                EiEvent::DeviceStopEmulating(_) => Ev::Stop,
                EiEvent::PointerMotion(m) => Ev::Motion(m.dx, m.dy),
                EiEvent::Button(b) => Ev::Button(b.button, b.state == ei::button::ButtonState::Press),
                EiEvent::KeyboardKey(k) => Ev::Key(k.key, k.state == ei::keyboard::KeyState::Press),
                EiEvent::KeyboardModifiers(m) => {
                    Ev::Mods { depressed: m.depressed, latched: m.latched, locked: m.locked }
                }
                EiEvent::ScrollDiscrete(s) => Ev::Discrete(s.discrete_dx, s.discrete_dy),
                EiEvent::ScrollDelta(s) => Ev::Delta(s.dx, s.dy),
                EiEvent::ScrollStop(s) => Ev::ScrollStop(s.x, s.y),
                EiEvent::Frame(_) => Ev::Frame,
                EiEvent::Disconnected(_) => {
                    let _ = tx.send(Ev::Disconnected);
                    return;
                }
                _ => continue,
            };
            if tx.send(out).is_err() {
                return;
            }
        }
    });
    rx
}

/// Drives the server until `pred` holds for the notes seen so far (or times out).
fn pump_until(server: &mut EisConn, mut pred: impl FnMut(&EisConn, &[Note]) -> bool) -> Vec<Note> {
    let mut notes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        notes.extend(server.read());
        if pred(server, &notes) {
            return notes;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out; notes so far: {notes:?}");
}

fn collect(rx: &mpsc::Receiver<Ev>, quiet: Duration) -> Vec<Ev> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.recv_timeout(quiet) {
        out.push(ev);
    }
    out
}

fn connected_pair(with_keymap: bool) -> (EisConn, mpsc::Receiver<Ev>) {
    let (server_end, client_end) = socketpair().unwrap();
    let mut server = EisConn::new(server_end).unwrap();
    if with_keymap {
        server.set_keymap(keymap());
    }
    let rx = spawn_client(client_end);
    pump_until(&mut server, |s, _| s.can_activate() && (!with_keymap || s.has_keyboard()));
    (server, rx)
}

#[test]
fn client_end_is_nonblocking_and_cloexec() {
    let (_server, client) = socketpair().unwrap();
    let fl = rustix::fs::fcntl_getfl(&client).unwrap();
    assert!(fl.contains(rustix::fs::OFlags::NONBLOCK));
    let fd = rustix::io::fcntl_getfd(&client).unwrap();
    assert!(fd.contains(rustix::io::FdFlags::CLOEXEC));
}

#[test]
fn devices_after_bind_with_keymap_and_full_activation() {
    let (mut server, rx) = connected_pair(true);
    let setup = collect(&rx, Duration::from_millis(100));
    assert_eq!(setup[0], Ev::SeatAdded);
    assert!(setup.contains(&Ev::DeviceAdded {
        name: "layercapture pointer".into(),
        keymap_size: None,
        keymap_nul: false
    }));
    assert!(setup.contains(&Ev::DeviceAdded {
        name: "layercapture keyboard".into(),
        keymap_size: Some(keymap().size() as u32),
        keymap_nul: true
    }));

    server.set_modifiers(Mods { depressed: 1, latched: 0, locked: 2, group: 0 });
    server.start_emulating(7);
    server.motion(3.0, -2.0);
    server.button(272, true);
    server.key(30, true);
    server.set_modifiers(Mods { depressed: 0, latched: 0, locked: 2, group: 0 });
    // Released while still held: stop_emulating must release key 30 and button 272 first.
    server.stop_emulating();
    assert_eq!(server.flush(), Flush::Done);

    let evs: Vec<Ev> = collect(&rx, Duration::from_millis(200)).into_iter().filter(|e| *e != Ev::Frame).collect();
    assert_eq!(
        evs,
        vec![
            Ev::Start(7),
            Ev::Start(7),
            Ev::Mods { depressed: 1, latched: 0, locked: 2 },
            Ev::Motion(3.0, -2.0),
            Ev::Button(272, true),
            Ev::Key(30, true),
            Ev::Mods { depressed: 0, latched: 0, locked: 2 },
            Ev::Key(30, false),
            Ev::Button(272, false),
            Ev::Stop,
            Ev::Stop,
        ]
    );
    // The ping sent after stop blocks the next activation until the pong arrives.
    assert!(!server.can_activate());
    pump_until(&mut server, |s, _| s.can_activate());
}

#[test]
fn no_keyboard_device_without_keymap_then_added() {
    let (mut server, rx) = connected_pair(false);
    let setup = collect(&rx, Duration::from_millis(100));
    assert!(!setup.iter().any(|e| matches!(e, Ev::DeviceAdded { name, .. } if name.contains("keyboard"))));
    assert!(!server.has_keyboard());
    assert!(server.set_keymap(keymap()));
    assert_eq!(server.flush(), Flush::Done);
    let later = collect(&rx, Duration::from_millis(100));
    assert!(later.iter().any(|e| matches!(e, Ev::DeviceAdded { keymap_nul: true, .. })));
}

#[test]
fn double_start_and_input_outside_activation_are_suppressed() {
    let (mut server, rx) = connected_pair(true);
    collect(&rx, Duration::from_millis(100));
    server.motion(1.0, 1.0); // not emulating: dropped
    server.start_emulating(1);
    server.start_emulating(2); // ignored
    server.key(30, false); // release without press: dropped
    server.stop_emulating();
    server.stop_emulating(); // no-op
    assert_eq!(server.flush(), Flush::Done);
    let evs: Vec<Ev> = collect(&rx, Duration::from_millis(200)).into_iter().filter(|e| *e != Ev::Frame).collect();
    assert_eq!(evs, vec![Ev::Start(1), Ev::Start(1), Ev::Stop, Ev::Stop]);
}

#[test]
fn wheel_is_discrete_only_and_finger_is_delta_plus_stop() {
    use wayland_client::protocol::wl_pointer::AxisSource;
    let (mut server, rx) = connected_pair(true);
    collect(&rx, Duration::from_millis(100));
    server.start_emulating(3);
    server.axis(&AxisFrame {
        source: Some(AxisSource::Wheel),
        value: [0.0, 15.0],
        has_value: [false, true],
        value120: [0, 120],
        stop: [false, false],
    });
    server.axis(&AxisFrame {
        source: Some(AxisSource::Finger),
        value: [0.0, 3.5],
        has_value: [false, true],
        value120: [0, 0],
        stop: [false, false],
    });
    server.axis(&AxisFrame { source: Some(AxisSource::Finger), stop: [false, true], ..Default::default() });
    server.stop_emulating();
    assert_eq!(server.flush(), Flush::Done);
    let evs: Vec<Ev> = collect(&rx, Duration::from_millis(200))
        .into_iter()
        .filter(|e| matches!(e, Ev::Discrete(..) | Ev::Delta(..) | Ev::ScrollStop(..)))
        .collect();
    assert_eq!(evs, vec![Ev::Discrete(0, 120), Ev::Delta(0.0, 3.5), Ev::ScrollStop(false, true)]);
}

#[test]
fn stalled_client_blocks_flush_but_connection_survives() {
    let (server_end, client_end) = socketpair().unwrap();
    let mut server = EisConn::new(server_end).unwrap();
    server.set_keymap(keymap());
    // A client that completes setup, then stops reading.
    let (pause_tx, pause_rx) = mpsc::channel::<()>();
    let rx = {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ctx = ei::Context::new(UnixStream::from(client_end)).unwrap();
            let (_c, events) = ctx.handshake_blocking("stall", ei::handshake::ContextType::Receiver).unwrap();
            for ev in events {
                match ev {
                    Ok(EiEvent::SeatAdded(s)) => {
                        s.seat.bind_capabilities(DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll);
                        let _ = ctx.flush();
                    }
                    Ok(EiEvent::DeviceStartEmulating(_)) => {
                        let _ = tx.send(());
                        // Stop reading until told to resume.
                        let _ = pause_rx.recv();
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });
        rx
    };
    pump_until(&mut server, |s, _| s.can_activate());
    server.start_emulating(1);
    assert_eq!(server.flush(), Flush::Done);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    let mut blocked = false;
    while started.elapsed() < Duration::from_secs(5) {
        server.motion(1.0, 0.0);
        if server.flush() == Flush::Blocked {
            blocked = true;
            break;
        }
    }
    assert!(blocked, "flush never blocked");
    assert!(server.backlog());
    assert!(!server.is_dead());
    assert!(!server.can_activate());
    // Client resumes: the backlog drains and the connection is still usable.
    pause_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.flush() != Flush::Done {
        assert!(Instant::now() < deadline, "backlog never drained");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!server.is_dead());
}

#[test]
fn parse_error_disconnects() {
    let (server_end, client_end) = socketpair().unwrap();
    let mut server = EisConn::new(server_end).unwrap();
    let mut raw = UnixStream::from(client_end);
    raw.set_nonblocking(false).unwrap();
    // A message header claiming a length smaller than the header itself.
    raw.write_all(&[0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    let notes = pump_until(&mut server, |s, _| s.is_dead());
    assert!(notes.iter().any(|n| matches!(n, Note::Disconnected { .. })), "{notes:?}");
}

/// KDE Connect's libei context has no name, and libei sends `ei_handshake.name` with a NULL
/// string. That must not kill the connection (it did on the first live test).
#[test]
fn null_handshake_name_is_tolerated() {
    let (server_end, client_end) = socketpair().unwrap();
    let mut server = EisConn::new(server_end).unwrap();
    let mut raw = UnixStream::from(client_end);
    raw.set_nonblocking(false).unwrap();
    let msg = |opcode: u32, args: &[u32]| {
        let mut m = Vec::new();
        m.extend_from_slice(&0u64.to_ne_bytes()); // ei_handshake is object 0
        m.extend_from_slice(&(16 + 4 * args.len() as u32).to_ne_bytes());
        m.extend_from_slice(&opcode.to_ne_bytes());
        for a in args {
            m.extend_from_slice(&a.to_ne_bytes());
        }
        m
    };
    raw.write_all(&msg(0, &[1])).unwrap(); // handshake_version 1
    raw.write_all(&msg(3, &[0])).unwrap(); // name: NULL string (length 0)
    raw.write_all(&msg(2, &[1])).unwrap(); // context_type receiver
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let notes = server.read();
        assert!(!notes.iter().any(|n| matches!(n, Note::Disconnected { .. })), "{notes:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!server.is_dead());
}

#[test]
fn eof_is_reported() {
    let (server_end, client_end) = socketpair().unwrap();
    let mut server = EisConn::new(server_end).unwrap();
    drop(client_end);
    let notes = pump_until(&mut server, |s, _| s.is_dead());
    assert!(notes.iter().any(|n| matches!(n, Note::Disconnected { .. })), "{notes:?}");
}
