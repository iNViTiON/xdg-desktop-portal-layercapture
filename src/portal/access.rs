//! Who may use the backend.
//!
//! - Method calls must come from the current owner of `org.freedesktop.portal.Desktop` (the
//!   xdg-desktop-portal frontend); anything else on the session bus is refused.
//! - Sessions are only granted to KDE Connect: the app id the frontend derives from the
//!   systemd unit (`org.kde.kdeconnect.daemon` for the autostarted daemon), or, when the app id
//!   is empty (daemon started some other way), a caller whose executable is kdeconnectd.
//!   `--allow-app-id` adds ids for testing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use zbus::fdo::{self, DBusProxy};
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName, UniqueName};
use zbus::Connection;

use crate::core::Cmd;

const FRONTEND: &str = "org.freedesktop.portal.Desktop";
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);

/// App ids KDE Connect runs under (same list as hypr-kdeconnect-portal).
const KDECONNECT_IDS: &[&str] = &[
    "org.kde.kdeconnect",
    "org.kde.kdeconnect.app",
    "org.kde.kdeconnect.daemon",
    "org.kde.kdeconnect.handler",
    "org.kde.kdeconnect.nonplasma",
    "org.kde.kdeconnect.sms",
];

pub struct Access {
    conn: Connection,
    owner: Mutex<Option<OwnedUniqueName>>,
    allow_app_ids: Vec<String>,
    trust_any_caller: bool,
}

impl Access {
    pub async fn new(conn: Connection, allow_app_ids: Vec<String>, trust_any_caller: bool) -> Self {
        let owner = lookup_owner(&conn).await;
        if trust_any_caller {
            tracing::warn!("--trust-any-caller: every caller and app id is accepted (testing only)");
        }
        Self { conn, owner: Mutex::new(owner), allow_app_ids, trust_any_caller }
    }

    fn cached_owner(&self) -> Option<OwnedUniqueName> {
        self.owner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_owner(&self, owner: Option<OwnedUniqueName>) {
        *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = owner;
    }

    /// Refuses callers other than the frontend.
    pub async fn check_frontend(&self, hdr: &Header<'_>) -> fdo::Result<()> {
        if self.trust_any_caller {
            return Ok(());
        }
        let Some(sender) = hdr.sender() else {
            return Err(fdo::Error::AccessDenied("no sender".into()));
        };
        if self.cached_owner().as_ref().map(|o| o.as_str()) == Some(sender.as_str()) {
            return Ok(());
        }
        // The owner may have changed before our NameOwnerChanged arrived: ask once.
        let fresh = lookup_owner(&self.conn).await;
        let ok = fresh.as_ref().map(|o| o.as_str()) == Some(sender.as_str());
        self.set_owner(fresh);
        if ok {
            Ok(())
        } else {
            tracing::warn!("refused call from {sender} (not the xdg-desktop-portal frontend)");
            Err(fdo::Error::AccessDenied("only xdg-desktop-portal may call this backend".into()))
        }
    }

    /// Decides whether `app_id` may create a session. `session_handle` encodes the client's
    /// unique name (`/org/freedesktop/portal/desktop/session/1_75/token` → `:1.75`).
    pub async fn check_app(&self, app_id: &str, session_handle: &str) -> Result<(), String> {
        if self.trust_any_caller {
            tracing::info!("CreateSession from app_id {app_id:?} (trust-any-caller)");
            return Ok(());
        }
        if KDECONNECT_IDS.contains(&app_id) || self.allow_app_ids.iter().any(|a| a == app_id) {
            tracing::info!("CreateSession from app_id {app_id:?}: allowed");
            return Ok(());
        }
        if !app_id.is_empty() {
            return Err(format!("app id {app_id:?} is not allowed"));
        }
        // Empty app id: identify the client process instead.
        let unique = client_unique_name(session_handle).ok_or("cannot tell the client from the session path")?;
        let pid = tokio::time::timeout(LOOKUP_TIMEOUT, async {
            let proxy = DBusProxy::new(&self.conn).await?;
            let name = BusName::Unique(UniqueName::try_from(unique.as_str()).map_err(zbus::Error::from)?);
            proxy.get_connection_unix_process_id(name).await.map_err(zbus::Error::from)
        })
        .await
        .map_err(|_| "PID lookup timed out".to_string())?
        .map_err(|e| format!("PID lookup failed: {e}"))?;
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|e| format!("reading /proc/{pid}/exe: {e}"))?;
        let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        tracing::info!("CreateSession with empty app_id from {unique} (pid {pid}, exe {})", exe.display());
        if name == "kdeconnectd" || name == ".kdeconnectd-wrapped" {
            Ok(())
        } else {
            Err(format!("client executable {} is not kdeconnectd", exe.display()))
        }
    }

    /// Follows the frontend's bus name; if it goes away, every session is closed.
    pub fn watch_frontend(self: Arc<Self>, tx: mpsc::UnboundedSender<Cmd>) {
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let proxy = match DBusProxy::new(&self.conn).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("watching {FRONTEND}: {e}");
                    return;
                }
            };
            let mut stream = match proxy.receive_name_owner_changed_with_args(&[(0, FRONTEND)]).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("watching {FRONTEND}: {e}");
                    return;
                }
            };
            // The owner may have changed between the initial lookup and the subscription.
            self.set_owner(lookup_owner(&self.conn).await);
            while let Some(sig) = stream.next().await {
                let Ok(args) = sig.args() else { continue };
                let new: Option<OwnedUniqueName> = Option::<UniqueName<'_>>::from(args.new_owner().clone()).map(Into::into);
                let had = Option::<UniqueName<'_>>::from(args.old_owner().clone()).is_some();
                tracing::info!("{FRONTEND} owner: {:?}", new.as_ref().map(|n| n.as_str()));
                self.set_owner(new.clone());
                if had && new.is_none() {
                    let _ = tx.send(Cmd::FrontendGone);
                }
            }
        });
    }
}

async fn lookup_owner(conn: &Connection) -> Option<OwnedUniqueName> {
    tokio::time::timeout(LOOKUP_TIMEOUT, async {
        let proxy = DBusProxy::new(conn).await.ok()?;
        let name = BusName::try_from(FRONTEND).ok()?;
        proxy.get_name_owner(name).await.ok()
    })
    .await
    .ok()
    .flatten()
}

/// `/org/freedesktop/portal/desktop/session/1_75/token` → `:1.75`.
fn client_unique_name(session_handle: &str) -> Option<String> {
    let rest = session_handle.strip_prefix("/org/freedesktop/portal/desktop/session/")?;
    let sender = rest.split('/').next()?;
    if sender.is_empty() || !sender.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(format!(":{}", sender.replace('_', ".")))
}

#[cfg(test)]
mod tests {
    use super::client_unique_name;

    #[test]
    fn unique_name_from_session_path() {
        assert_eq!(
            client_unique_name("/org/freedesktop/portal/desktop/session/1_75/kdeconnect_shareinputdevices1852926742").as_deref(),
            Some(":1.75")
        );
        assert_eq!(client_unique_name("/org/elsewhere/1_2/x"), None);
    }
}
