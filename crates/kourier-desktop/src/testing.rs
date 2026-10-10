//! Test support (cargo feature `testing`): a private message bus and mock desktop services, so
//! tests never touch the user's session bus, never show a real notification and never put an
//! icon into the real tray.
//!
//! [`PrivateBus`] starts `dbus-daemon` with a minimal configuration: no service directories, so
//! nothing on it can be auto-activated (a stock session config would start the desktop's real
//! notification daemon on demand).

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use crate::{Bus, DesktopError};

/// A `dbus-daemon` of its own, stopped on drop.
#[derive(Debug)]
pub struct PrivateBus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    /// Starts the daemon and waits for its address.
    pub fn start() -> std::io::Result<Self> {
        let dir = tempfile::Builder::new().prefix("kourier-bus-").tempdir()?;
        let config = dir.path().join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir.path().display()
            ),
        )?;
        let mut child = Command::new(dbus_daemon())
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("dbus-daemon without stdout"))?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line)?;
        let address = line.trim().to_owned();
        if address.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("dbus-daemon printed no address"));
        }
        Ok(Self {
            child,
            address,
            _dir: dir,
        })
    }

    /// The bus address (`unix:path=…,guid=…`), for `DBUS_SESSION_BUS_ADDRESS`.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// A new connection to this bus.
    pub async fn connect(&self) -> Result<Bus, DesktopError> {
        Bus::at(&self.address).await
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn dbus_daemon() -> PathBuf {
    ["/usr/bin/dbus-daemon", "/bin/dbus-daemon"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("dbus-daemon"))
}

/// One `Notify` call the mock server received.
#[derive(Clone, Debug)]
pub struct NotifyCall {
    /// `app_name`.
    pub app_name: String,
    /// `replaces_id`.
    pub replaces_id: u32,
    /// `app_icon`.
    pub icon: String,
    /// `summary`.
    pub summary: String,
    /// `body` as sent.
    pub body: String,
    /// `actions` (key, label, key, label, …).
    pub actions: Vec<String>,
    /// `hints`.
    pub hints: HashMap<String, OwnedValue>,
    /// `expire_timeout`.
    pub timeout: i32,
    /// The id the server answered.
    pub id: u32,
}

impl NotifyCall {
    /// A string hint.
    pub fn hint_str(&self, key: &str) -> Option<String> {
        let v = self.hints.get(key)?;
        <&str>::try_from(v).ok().map(str::to_owned)
    }

    /// Whether the action list has `key`.
    pub fn has_action(&self, key: &str) -> bool {
        self.actions
            .chunks(2)
            .any(|c| c.first().map(String::as_str) == Some(key))
    }
}

/// What the mock notification server saw.
#[derive(Debug, Default)]
pub struct NotificationLog {
    /// Every `Notify`, in order.
    pub calls: Vec<NotifyCall>,
    /// Every `CloseNotification` id, in order.
    pub closed: Vec<u32>,
    /// Ids currently shown.
    pub open: Vec<u32>,
    /// How many `GetCapabilities` calls arrived (a client connecting).
    pub capability_calls: u32,
}

/// A mock `org.freedesktop.Notifications` server that records calls and emits signals on
/// request.
#[derive(Clone, Debug)]
pub struct MockNotifications {
    bus: Bus,
    log: Arc<Mutex<NotificationLog>>,
    changed: Arc<Notify>,
}

struct Server {
    log: Arc<Mutex<NotificationLog>>,
    changed: Arc<Notify>,
    next: u32,
    caps: Vec<String>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let id = if replaces_id != 0 && log.open.contains(&replaces_id) {
            replaces_id
        } else {
            self.next += 1;
            log.open.push(self.next);
            self.next
        };
        log.calls.push(NotifyCall {
            app_name,
            replaces_id,
            icon: app_icon,
            summary,
            body,
            actions,
            hints,
            timeout: expire_timeout,
            id,
        });
        drop(log);
        self.changed.notify_waiters();
        id
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let was_open = {
            let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
            log.closed.push(id);
            let was = log.open.contains(&id);
            log.open.retain(|o| *o != id);
            was
        };
        self.changed.notify_waiters();
        if was_open {
            Self::notification_closed(&emitter, id, 3).await?;
        }
        Ok(())
    }

    fn get_capabilities(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .capability_calls += 1;
        self.changed.notify_waiters();
        self.caps.clone()
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        ("mock".into(), "kourier".into(), "1".into(), "1.2".into())
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

    #[zbus(signal)]
    async fn activation_token(
        emitter: &SignalEmitter<'_>,
        id: u32,
        activation_token: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn notification_replied(
        emitter: &SignalEmitter<'_>,
        id: u32,
        text: &str,
    ) -> zbus::Result<()>;
}

/// Plasma's capabilities (as of Plasma 6).
pub const PLASMA_CAPS: [&str; 8] = [
    "body",
    "body-hyperlinks",
    "body-markup",
    "body-images",
    "icon-static",
    "actions",
    "persistence",
    "inline-reply",
];

impl MockNotifications {
    /// Serves the mock on a new connection to `bus_address` with `caps` and takes the name.
    pub async fn start(bus_address: &str, caps: &[&str]) -> Result<Self, DesktopError> {
        let bus = Bus::at(bus_address).await?;
        let log = Arc::new(Mutex::new(NotificationLog::default()));
        let changed = Arc::new(Notify::new());
        bus.connection()
            .object_server()
            .at(
                crate::notifications::PATH,
                Server {
                    log: log.clone(),
                    changed: changed.clone(),
                    next: 0,
                    caps: caps.iter().map(|c| (*c).to_owned()).collect(),
                },
            )
            .await?;
        bus.connection()
            .request_name(crate::notifications::SERVICE)
            .await?;
        Ok(Self { bus, log, changed })
    }

    /// Runs `f` on the log.
    pub fn with_log<R>(&self, f: impl FnOnce(&NotificationLog) -> R) -> R {
        f(&self.log.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Waits (up to `timeout`) until `cond` holds for the log.
    pub async fn wait_for(
        &self,
        timeout: std::time::Duration,
        cond: impl Fn(&NotificationLog) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            if self.with_log(&cond) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.with_log(&cond);
            }
        }
    }

    async fn emitter(&self) -> Result<SignalEmitter<'static>, DesktopError> {
        Ok(SignalEmitter::new(
            self.bus.connection(),
            crate::notifications::PATH,
        )?)
    }

    /// The user clicked: `ActivationToken(id, token)` then `ActionInvoked(id, key)`.
    pub async fn invoke(
        &self,
        id: u32,
        key: &str,
        token: Option<&str>,
    ) -> Result<(), DesktopError> {
        let emitter = self.emitter().await?;
        if let Some(token) = token {
            Server::activation_token(&emitter, id, token).await?;
        }
        Server::action_invoked(&emitter, id, key).await?;
        Ok(())
    }

    /// The user sent an inline reply.
    pub async fn reply(&self, id: u32, text: &str) -> Result<(), DesktopError> {
        let emitter = self.emitter().await?;
        Server::notification_replied(&emitter, id, text).await?;
        Ok(())
    }

    /// The server goes away (releases its name), like a quitting plasmashell.
    pub async fn release(&self) -> Result<(), DesktopError> {
        self.bus
            .connection()
            .release_name(crate::notifications::SERVICE)
            .await?;
        Ok(())
    }

    /// The user dismissed a notification.
    pub async fn dismiss(&self, id: u32) -> Result<(), DesktopError> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .open
            .retain(|o| *o != id);
        let emitter = self.emitter().await?;
        Server::notification_closed(&emitter, id, 2).await?;
        Ok(())
    }
}

/// What the mock StatusNotifierWatcher saw.
#[derive(Debug, Default)]
pub struct WatcherLog {
    /// Registered item service names.
    pub items: Vec<String>,
}

struct Watcher {
    log: Arc<Mutex<WatcherLog>>,
    changed: Arc<Notify>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        // ksni passes its well-known name; a bare path means "the sender".
        let name = if service.starts_with('/') {
            header.sender().map(ToString::to_string).unwrap_or(service)
        } else {
            service
        };
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .items
            .push(name);
        self.changed.notify_waiters();
    }

    fn register_status_notifier_host(&self, _service: String) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .items
            .clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

/// A mock `org.kde.StatusNotifierWatcher` (the tray host side).
#[derive(Clone, Debug)]
pub struct MockWatcher {
    bus: Bus,
    log: Arc<Mutex<WatcherLog>>,
    changed: Arc<Notify>,
}

impl MockWatcher {
    /// Serves the watcher on a new connection to `bus_address` and takes its name.
    pub async fn start(bus_address: &str) -> Result<Self, DesktopError> {
        let bus = Bus::at(bus_address).await?;
        let log = Arc::new(Mutex::new(WatcherLog::default()));
        let changed = Arc::new(Notify::new());
        bus.connection()
            .object_server()
            .at(
                "/StatusNotifierWatcher",
                Watcher {
                    log: log.clone(),
                    changed: changed.clone(),
                },
            )
            .await?;
        bus.connection()
            .request_name("org.kde.StatusNotifierWatcher")
            .await?;
        Ok(Self { bus, log, changed })
    }

    /// The connection the watcher serves on (to call into registered items).
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// Waits (up to `timeout`) for the first registered item and returns its service name.
    pub async fn wait_for_item(&self, timeout: std::time::Duration) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            if let Some(item) = self
                .log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .items
                .first()
                .cloned()
            {
                return Some(item);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return None;
            }
        }
    }

    /// Calls a method of the item at `service` (`org.kde.StatusNotifierItem` at
    /// `/StatusNotifierItem`, or `com.canonical.dbusmenu` at `/MenuBar`).
    pub async fn call_item<B>(
        &self,
        service: &str,
        path: &str,
        interface: &str,
        method: &str,
        body: &B,
    ) -> Result<(), DesktopError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.bus
            .connection()
            .call_method(Some(service), path, Some(interface), method, body)
            .await?;
        Ok(())
    }

    /// Reads a string property of the item at `service`.
    pub async fn item_property(&self, service: &str, name: &str) -> Result<String, DesktopError> {
        let proxy = zbus::fdo::PropertiesProxy::builder(self.bus.connection())
            .destination(service.to_owned())?
            .path("/StatusNotifierItem")?
            .build()
            .await?;
        let iface = zbus::names::InterfaceName::try_from("org.kde.StatusNotifierItem")
            .map_err(|e| DesktopError::Dbus(e.to_string()))?;
        let value = proxy.get(iface, name).await?;
        <&str>::try_from(&value)
            .map(str::to_owned)
            .map_err(|e| DesktopError::Dbus(e.to_string()))
    }
}

/// Mock logind and NetworkManager for [`crate::hints`]: a connection that can own their bus
/// names and emits their signals, on a private bus standing in for the system bus. Nothing
/// here ever talks to the machine's own logind or NetworkManager.
#[derive(Clone, Debug)]
pub struct MockSystem {
    bus: Bus,
}

impl MockSystem {
    /// A connection to `bus_address` that owns `org.freedesktop.login1` and
    /// `org.freedesktop.NetworkManager`.
    pub async fn start(bus_address: &str) -> Result<Self, DesktopError> {
        let mock = Self::impostor(bus_address).await?;
        for name in [crate::hints::LOGIND, crate::hints::NETWORK_MANAGER] {
            mock.bus.connection().request_name(name).await?;
        }
        Ok(mock)
    }

    /// A connection that owns neither name: whatever it emits must not count.
    pub async fn impostor(bus_address: &str) -> Result<Self, DesktopError> {
        Ok(Self {
            bus: Bus::at(bus_address).await?,
        })
    }

    async fn emit<B>(
        &self,
        path: &str,
        interface: &str,
        member: &str,
        body: &B,
    ) -> Result<(), DesktopError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.bus
            .connection()
            .emit_signal(None::<&str>, path, interface, member, body)
            .await?;
        Ok(())
    }

    /// logind's `PrepareForSleep`: `true` before the suspend, `false` after the resume.
    pub async fn prepare_for_sleep(&self, start: bool) -> Result<(), DesktopError> {
        self.emit(
            crate::hints::LOGIND_PATH,
            crate::hints::LOGIND_MANAGER,
            "PrepareForSleep",
            &start,
        )
        .await
    }

    /// NetworkManager's `StateChanged` (70 = connected globally, 20 = disconnected).
    pub async fn network_state(&self, state: u32) -> Result<(), DesktopError> {
        self.emit(
            crate::hints::NETWORK_MANAGER_PATH,
            crate::hints::NETWORK_MANAGER,
            "StateChanged",
            &state,
        )
        .await
    }

    /// `PropertiesChanged` of NetworkManager's main object for one property (an object path
    /// as its new value, as `PrimaryConnection` has).
    pub async fn property_changed(&self, property: &str) -> Result<(), DesktopError> {
        let value = zbus::zvariant::Value::from(
            zbus::zvariant::ObjectPath::try_from("/org/freedesktop/NetworkManager/Active/7")
                .map_err(|e| DesktopError::Dbus(e.to_string()))?,
        );
        let changed: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::from([(property, value)]);
        self.emit(
            crate::hints::NETWORK_MANAGER_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(crate::hints::NETWORK_MANAGER, changed, Vec::<&str>::new()),
        )
        .await
    }
}
