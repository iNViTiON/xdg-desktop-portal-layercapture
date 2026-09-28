//! D-Bus side: `org.freedesktop.impl.portal.InputCapture` at `/org/freedesktop/portal/desktop`,
//! one `org.freedesktop.impl.portal.Session` per session, and the signal emitter.
//!
//! Rules from xdg-desktop-portal 1.20.4 (see the plan's portal checklist):
//! - every interface uses `spawn = false`, so calls from one session are handled in order;
//! - handlers never fail in ways the frontend turns into Session.Closed (kdeconnectd then
//!   crashes): GetZones/SetPointerBarriers answer response 0 even for unknown sessions or a
//!   stalled core, and ConnectToEIS never returns an error (the frontend would use an
//!   uninitialised fd);
//! - objects are served before the bus name is requested, so the frontend's startup GetAll
//!   sees `SupportedCapabilities = 3`;
//! - only the current owner of `org.freedesktop.portal.Desktop` may call methods; properties
//!   are unrestricted (the frontend reads them before it owns its name).

pub mod access;
pub mod barriers;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, fdo, interface};

use crate::core::{Cmd, Signal, ZonesReply};
use access::Access;

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.layercapture";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const CONTROL_PATH: &str = "/org/freedesktop/portal/desktop/layercapture";
/// How long a handler waits for the core before answering a safe default.
const CORE_TIMEOUT: Duration = Duration::from_secs(2);

type Results = HashMap<String, OwnedValue>;

fn ov<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("value without fds")
}

async fn ask<T>(tx: &mpsc::UnboundedSender<Cmd>, make: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> Option<T> {
    let (reply, rx) = oneshot::channel();
    tx.send(make(reply)).ok()?;
    match tokio::time::timeout(CORE_TIMEOUT, rx).await {
        Ok(Ok(v)) => Some(v),
        _ => {
            tracing::error!("core did not answer within {} s; replying with a safe default", CORE_TIMEOUT.as_secs());
            None
        }
    }
}

pub struct InputCapture {
    tx: mpsc::UnboundedSender<Cmd>,
    access: Arc<Access>,
}

#[interface(name = "org.freedesktop.impl.portal.InputCapture", spawn = false)]
impl InputCapture {
    #[allow(clippy::too_many_arguments)]
    async fn create_session(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
        handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        self.access.check_frontend(&hdr).await?;
        let caps = match options.get("capabilities").map(|v| &**v) {
            Some(Value::U32(c)) => *c,
            _ => 0,
        };
        let _ = handle;
        if let Err(why) = self.access.check_app(&app_id, session_handle.as_str()).await {
            tracing::warn!("CreateSession denied for app_id {app_id:?}: {why}");
            return Ok((2, Results::new()));
        }
        let session = session_handle.to_string();
        let granted = ask(&self.tx, |reply| Cmd::CreateSession { session: session.clone(), app_id, caps, reply })
            .await
            .unwrap_or(caps & 3);
        let obj = SessionObj { tx: self.tx.clone(), access: self.access.clone(), id: session };
        if let Err(e) = server.at(session_handle.as_ref(), obj).await {
            tracing::warn!("exporting session object: {e}");
        }
        let mut results = Results::new();
        // The frontend requires this key on success.
        results.insert("capabilities".into(), ov(granted));
        Ok((0, results))
    }

    async fn get_zones(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        self.access.check_frontend(&hdr).await?;
        let session = session_handle.to_string();
        let z = ask(&self.tx, |reply| Cmd::GetZones { session, reply })
            .await
            .unwrap_or(ZonesReply { zones: vec![(1920, 1080, 0, 0)], zone_set: 1 });
        let mut results = Results::new();
        results.insert("zones".into(), ov(z.zones));
        results.insert("zone_set".into(), ov(z.zone_set));
        Ok((0, results))
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_pointer_barriers(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
    ) -> fdo::Result<(u32, Results)> {
        self.access.check_frontend(&hdr).await?;
        let parsed: Vec<_> = barriers.iter().map(barriers::parse_barrier).collect();
        let all_ids: Vec<u32> = parsed
            .iter()
            .filter_map(|b| match b {
                Ok((id, _)) => Some(*id),
                Err(id) => *id,
            })
            .collect();
        let session = session_handle.to_string();
        let failed = ask(&self.tx, |reply| Cmd::SetBarriers { session, barriers: parsed, zone_set, reply })
            .await
            .unwrap_or(all_ids);
        let mut results = Results::new();
        results.insert("failed_barriers".into(), ov(failed));
        Ok((0, results))
    }

    async fn enable(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        self.access.check_frontend(&hdr).await?;
        let _ = self.tx.send(Cmd::Enable { session: session_handle.to_string() });
        Ok((0, Results::new()))
    }

    async fn disable(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        self.access.check_frontend(&hdr).await?;
        let _ = self.tx.send(Cmd::Disable { session: session_handle.to_string() });
        Ok((0, Results::new()))
    }

    async fn release(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(u32, Results)> {
        self.access.check_frontend(&hdr).await?;
        let activation_id = match options.get("activation_id").map(|v| &**v) {
            Some(Value::U32(id)) => Some(*id),
            _ => None,
        };
        let cursor = options.get("cursor_position").and_then(|v| <(f64, f64)>::try_from(v.clone()).ok());
        let _ = self.tx.send(Cmd::Release { session: session_handle.to_string(), activation_id, cursor });
        Ok((0, Results::new()))
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedFd> {
        self.access.check_frontend(&hdr).await?;
        // Never an error for the frontend: an unknown session gets a socket whose other end
        // the core simply drops (immediate EOF).
        match crate::eis::socketpair() {
            Ok((server, client)) => {
                let _ = self.tx.send(Cmd::ConnectEis { session: session_handle.to_string(), server });
                Ok(OwnedFd::from(client))
            }
            Err(e) => {
                // Still hand out a valid fd (reads give EOF): an error reply makes the frontend
                // use an uninitialised fd.
                tracing::error!("ConnectToEIS: {e:#}; returning /dev/null");
                let null = std::fs::File::open("/dev/null").map_err(|e| fdo::Error::Failed(e.to_string()))?;
                Ok(OwnedFd::from(std::os::fd::OwnedFd::from(null)))
            }
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_capabilities(&self) -> u32 {
        3 // KEYBOARD | POINTER
    }

    #[zbus(property(emits_changed_signal = "const"), name = "version")]
    fn version(&self) -> u32 {
        1
    }

    #[zbus(signal)]
    async fn activated(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn deactivated(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn zones_changed(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    // Never emitted (KDE Connect never re-enables after it), but part of the interface.
    #[zbus(signal)]
    async fn disabled(emitter: &SignalEmitter<'_>, session_handle: ObjectPath<'_>, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

pub struct SessionObj {
    tx: mpsc::UnboundedSender<Cmd>,
    access: Arc<Access>,
    id: String,
}

#[interface(name = "org.freedesktop.impl.portal.Session", spawn = false)]
impl SessionObj {
    async fn close(&self, #[zbus(header)] hdr: Header<'_>, #[zbus(connection)] conn: &Connection) -> fdo::Result<()> {
        self.access.check_frontend(&hdr).await?;
        let _ = self.tx.send(Cmd::Close { session: self.id.clone() });
        // Our own object cannot be removed while its method runs; do it right after.
        let conn = conn.clone();
        let path = self.id.clone();
        tokio::spawn(async move {
            let _ = conn.object_server().remove::<SessionObj, _>(path.as_str()).await;
        });
        Ok(())
    }

    // Never emitted: kdeconnectd dereferences its session after Closed.
    #[zbus(signal)]
    async fn closed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property(emits_changed_signal = "const"), name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Dev-only control interface (`--dev-control`) for the isolated contract test.
pub struct Control {
    tx: mpsc::UnboundedSender<Cmd>,
}

#[interface(name = "org.freedesktop.impl.portal.desktop.layercapture.Control", spawn = false)]
impl Control {
    async fn simulate_activation(&self, session_handle: OwnedObjectPath) -> bool {
        ask(&self.tx, |reply| Cmd::SimulateActivation { session: session_handle.to_string(), reply })
            .await
            .unwrap_or(false)
    }

    async fn stall_core(&self, millis: u32) {
        let _ = self.tx.send(Cmd::Stall(Duration::from_millis(millis as u64)));
    }
}

pub struct Options {
    pub allow_app_ids: Vec<String>,
    pub trust_any_caller: bool,
    pub dev_control: bool,
}

/// Serves the portal objects, then takes the bus name. Returns the connection, the command
/// receiver for the core and the signal sender for it.
pub async fn start(
    opts: Options,
) -> Result<(Connection, mpsc::UnboundedReceiver<Cmd>, mpsc::UnboundedSender<Signal>)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let (sig_tx, sig_rx) = mpsc::unbounded_channel();
    let conn = zbus::connection::Builder::session()?.build().await.context("connecting to the session bus")?;
    let access = Arc::new(Access::new(conn.clone(), opts.allow_app_ids, opts.trust_any_caller).await);
    conn.object_server()
        .at(PORTAL_PATH, InputCapture { tx: tx.clone(), access: access.clone() })
        .await?;
    if opts.dev_control {
        tracing::warn!("dev control interface enabled at {CONTROL_PATH}");
        conn.object_server().at(CONTROL_PATH, Control { tx: tx.clone() }).await?;
    }
    conn.request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await
        .with_context(|| format!("taking the bus name {BUS_NAME} (already running?)"))?;
    tracing::info!("serving {BUS_NAME}");
    access.clone().watch_frontend(tx.clone());
    tokio::spawn(emit_signals(conn.clone(), sig_rx));
    Ok((conn, rx, sig_tx))
}

async fn emit_signals(conn: Connection, mut rx: mpsc::UnboundedReceiver<Signal>) {
    let emitter = match SignalEmitter::new(&conn, PORTAL_PATH) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("signal emitter: {e}");
            return;
        }
    };
    while let Some(sig) = rx.recv().await {
        let res = match &sig {
            Signal::Activated { session, activation_id, cursor, barrier_id } => {
                let Ok(path) = ObjectPath::try_from(session.as_str()) else { continue };
                let mut o: HashMap<&str, Value<'_>> = HashMap::new();
                o.insert("activation_id", Value::from(*activation_id));
                o.insert("cursor_position", Value::from(*cursor));
                o.insert("barrier_id", Value::from(*barrier_id));
                InputCapture::activated(&emitter, path, o).await
            }
            Signal::Deactivated { session, activation_id, cursor } => {
                let Ok(path) = ObjectPath::try_from(session.as_str()) else { continue };
                let mut o: HashMap<&str, Value<'_>> = HashMap::new();
                o.insert("activation_id", Value::from(*activation_id));
                o.insert("cursor_position", Value::from(*cursor));
                InputCapture::deactivated(&emitter, path, o).await
            }
            Signal::ZonesChanged { session, zone_set } => {
                let Ok(path) = ObjectPath::try_from(session.as_str()) else { continue };
                let mut o: HashMap<&str, Value<'_>> = HashMap::new();
                o.insert("zone_set", Value::from(*zone_set));
                InputCapture::zones_changed(&emitter, path, o).await
            }
        };
        match res {
            Ok(()) => tracing::debug!("emitted {sig:?}"),
            Err(e) => tracing::error!("emitting {sig:?}: {e}"),
        }
    }
}
