//! The session bus side: the notification server and the appearance part
//! of the desktop portal settings, which Chromium and libadwaita follow
//! live.

use std::collections::HashMap;

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};
use zbus::{OwnedVariant, Variant};

use crate::wm::{Cmd, Handle};

/// What the window manager tells the bus.
pub enum Event {
    Closed { id: u32, reason: u32 },
    Action { id: u32, key: String },
    Appearance(bool),
}

/// A notification as a client sent it.
pub struct Note {
    pub app: String,
    pub icon: String,
    pub entry: Option<String>,
    pub summary: String,
    pub body: String,
    /// Pairs of key and label.
    pub actions: Vec<(String, String)>,
    pub timeout_ms: i32,
    pub replaces: u32,
}

const NOTIFICATIONS: &str = "/org/freedesktop/Notifications";
const PORTAL: &str = "/org/freedesktop/portal/desktop";

struct Notifications {
    wm: Handle,
}

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "actions".into()]
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedVariant>,
        expire_timeout: i32,
    ) -> fdo::Result<u32> {
        let entry = hints
            .get("desktop-entry")
            .and_then(|v| String::try_from(v.clone()).ok());
        let note = Note {
            app: app_name,
            icon: app_icon,
            entry,
            summary,
            body,
            actions: actions
                .chunks(2)
                .filter(|p| p.len() == 2)
                .map(|p| (p[0].clone(), p[1].clone()))
                .collect(),
            timeout_ms: expire_timeout,
            replaces: replaces_id,
        };
        match self.wm.call(|r| Cmd::Notify(Box::new(note), r)).await {
            Ok(v) => Ok(v["id"].as_u64().unwrap_or(0) as u32),
            Err((_, e)) => Err(fdo::Error::Failed(e)),
        }
    }

    async fn close_notification(&self, id: u32) -> fdo::Result<()> {
        self.wm
            .call(|r| Cmd::CloseNote(id, r))
            .await
            .map(|_| ())
            .map_err(|(_, e)| fdo::Error::Failed(e))
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "shuru-desktop".into(),
            "SuperHQ".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

/// Reads the appearance the window manager is in, so a client that asks
/// right after the switch already gets the new value.
struct Settings;

/// org.freedesktop.appearance color-scheme: 1 prefers dark, 2 light.
fn scheme(dark: bool) -> u32 {
    if dark {
        1
    } else {
        2
    }
}

impl Settings {
    fn lookup(&self, namespace: &str, key: &str) -> fdo::Result<OwnedVariant> {
        if namespace == "org.freedesktop.appearance" && key == "color-scheme" {
            return Ok(OwnedVariant::from(scheme(crate::draw::is_dark())));
        }
        Err(fdo::Error::Failed(format!("no setting {namespace} {key}")))
    }
}

#[interface(name = "org.freedesktop.portal.Settings")]
impl Settings {
    /// The first version of the call, which wraps the value once more.
    fn read(&self, namespace: &str, key: &str) -> fdo::Result<OwnedVariant> {
        let v = self.lookup(namespace, key)?;
        OwnedVariant::try_from(Variant::Variant(Box::new(Variant::from(v))))
            .map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    fn read_one(&self, namespace: &str, key: &str) -> fdo::Result<OwnedVariant> {
        self.lookup(namespace, key)
    }

    fn read_all(&self, namespaces: Vec<String>) -> HashMap<String, HashMap<String, OwnedVariant>> {
        let wanted = |ns: &str| {
            namespaces.is_empty()
                || namespaces.iter().any(|p| match p.strip_suffix('*') {
                    Some(prefix) => ns.starts_with(prefix),
                    None => ns == p,
                })
        };
        let mut out = HashMap::new();
        if wanted("org.freedesktop.appearance") {
            let mut keys = HashMap::new();
            keys.insert(
                "color-scheme".to_string(),
                OwnedVariant::from(scheme(crate::draw::is_dark())),
            );
            out.insert("org.freedesktop.appearance".to_string(), keys);
        }
        out
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }

    #[zbus(signal)]
    async fn setting_changed(
        emitter: &SignalEmitter<'_>,
        namespace: &str,
        key: &str,
        value: Variant<'_>,
    ) -> zbus::Result<()>;
}

/// Serves both on the session bus until the window manager stops. Without
/// a session bus there is nothing to do.
pub async fn run(wm: Handle, mut events: mpsc::UnboundedReceiver<Event>) {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        eprintln!("shuru-desktop: no session bus, so no notifications or portal settings");
        return;
    }
    let conn = zbus::connection::Builder::session()
        .name("org.freedesktop.Notifications")
        .name("org.freedesktop.portal.Desktop")
        .serve_at(NOTIFICATIONS, Notifications { wm })
        .serve_at(PORTAL, Settings)
        .build()
        .await;
    let conn = match conn {
        Ok(c) => c,
        Err(e) => {
            eprintln!("shuru-desktop: session bus: {e}");
            return;
        }
    };
    let server = conn.object_server();
    while let Some(ev) = events.recv().await {
        let sent = match ev {
            Event::Closed { id, reason } => {
                match server.interface::<_, Notifications>(NOTIFICATIONS).await {
                    Ok(i) => {
                        Notifications::notification_closed(i.signal_emitter(), id, reason).await
                    }
                    Err(e) => Err(e),
                }
            }
            Event::Action { id, key } => {
                match server.interface::<_, Notifications>(NOTIFICATIONS).await {
                    Ok(i) => Notifications::action_invoked(i.signal_emitter(), id, &key).await,
                    Err(e) => Err(e),
                }
            }
            Event::Appearance(dark) => match server.interface::<_, Settings>(PORTAL).await {
                Ok(i) => {
                    Settings::setting_changed(
                        i.signal_emitter(),
                        "org.freedesktop.appearance",
                        "color-scheme",
                        Variant::from(scheme(dark)),
                    )
                    .await
                }
                Err(e) => Err(e),
            },
        };
        if let Err(e) = sent {
            eprintln!("shuru-desktop: session bus signal: {e}");
        }
    }
}
