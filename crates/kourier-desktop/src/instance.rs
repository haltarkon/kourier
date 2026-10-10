//! Single instance (architecture §8.2 step 1, §8.8): the first process owns a well-known bus
//! name and serves `org.freedesktop.Application` on it; a later launch finds the name taken and
//! forwards its activation (with `$XDG_ACTIVATION_TOKEN`) or its `sgnl://` URIs, then exits.
//!
//! The interface object is registered before the name is requested, so a forwarded call can
//! never arrive at a name without an object.

use std::collections::HashMap;

use tokio::sync::mpsc;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::{OwnedValue, Value};

use crate::{Bus, DesktopError};

/// The `platform_data` key of an XDG activation token.
pub const TOKEN_KEY: &str = "activation-token";
/// The X11 startup-notification key some launchers still send.
pub const STARTUP_ID_KEY: &str = "desktop-startup-id";
/// Most URIs one `Open` call may carry; more are dropped. A bound on what another process
/// can make the running instance hold (security), not a rule of any network.
pub const MAX_URIS: usize = 16;
/// Longest URI accepted; longer ones are dropped. A bound on input from other processes
/// (security), with room to spare for the longest link Kourier is handed: a solved challenge
/// (`signalcaptcha://…`) carries the captcha provider's token, about 2,600 bytes today and of
/// no documented maximum. Signal Desktop sets no limit on that link
/// (`SD/ts/util/signalRoutes.std.ts`, the captcha route), so a tight one here would be a limit
/// of Kourier's own that breaks the hand-off from the browser the day tokens grow.
pub const MAX_URI_LEN: usize = 16 * 1024;

/// What another launch (or the desktop) asks the running instance to do.
#[derive(Clone, PartialEq, Eq)]
pub enum InstanceRequest {
    /// Show and raise the window.
    Activate {
        /// The XDG activation token that allows raising it on Wayland.
        token: Option<String>,
    },
    /// Open URIs (`sgnl://…`).
    Open {
        /// The URIs (at most [`MAX_URIS`], each at most [`MAX_URI_LEN`] bytes).
        uris: Vec<String>,
        /// The activation token.
        token: Option<String>,
    },
    /// A desktop action (`ActivateAction`) by name.
    Action {
        /// The action name.
        name: String,
        /// The activation token.
        token: Option<String>,
    },
}

// URIs and tokens are never logged.
impl std::fmt::Debug for InstanceRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Activate { token } => f
                .debug_struct("Activate")
                .field("token", &token.is_some())
                .finish(),
            Self::Open { uris, token } => f
                .debug_struct("Open")
                .field("uris", &uris.len())
                .field("token", &token.is_some())
                .finish(),
            Self::Action { name, token } => f
                .debug_struct("Action")
                .field("name", name)
                .field("token", &token.is_some())
                .finish(),
        }
    }
}

/// The D-Bus object path of `org.freedesktop.Application` for a bus name
/// (`app.kourier.Kourier` → `/app/kourier/Kourier`).
pub fn object_path(name: &str) -> String {
    let mut path = String::with_capacity(name.len() + 1);
    for element in name.split('.') {
        path.push('/');
        path.extend(element.chars().map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        }));
    }
    path
}

fn token_of(platform_data: &HashMap<String, OwnedValue>) -> Option<String> {
    [TOKEN_KEY, STARTUP_ID_KEY].iter().find_map(|key| {
        let value = platform_data.get(*key)?;
        let s = <&str>::try_from(value).ok()?;
        (!s.is_empty() && s.len() <= MAX_URI_LEN).then(|| s.to_owned())
    })
}

/// The `org.freedesktop.Application` object of the running instance.
struct Application {
    tx: mpsc::UnboundedSender<InstanceRequest>,
}

#[zbus::interface(name = "org.freedesktop.Application")]
impl Application {
    fn activate(&self, platform_data: HashMap<String, OwnedValue>) {
        let _ = self.tx.send(InstanceRequest::Activate {
            token: token_of(&platform_data),
        });
    }

    fn open(&self, uris: Vec<String>, platform_data: HashMap<String, OwnedValue>) {
        let uris: Vec<String> = uris
            .into_iter()
            .filter(|u| !u.is_empty() && u.len() <= MAX_URI_LEN)
            .take(MAX_URIS)
            .collect();
        let _ = self.tx.send(InstanceRequest::Open {
            uris,
            token: token_of(&platform_data),
        });
    }

    fn activate_action(
        &self,
        action_name: String,
        _parameter: Vec<OwnedValue>,
        platform_data: HashMap<String, OwnedValue>,
    ) {
        if action_name.len() <= 256 {
            let _ = self.tx.send(InstanceRequest::Action {
                name: action_name,
                token: token_of(&platform_data),
            });
        }
    }
}

/// The running (first) instance: owns the bus name while it lives.
#[derive(Debug)]
pub struct Instance {
    bus: Bus,
    name: String,
    requests: Option<mpsc::UnboundedReceiver<InstanceRequest>>,
}

impl Instance {
    /// The connection that owns the name.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// The owned bus name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The requests of later launches (once; `None` afterwards).
    pub fn take_requests(&mut self) -> Option<mpsc::UnboundedReceiver<InstanceRequest>> {
        self.requests.take()
    }
}

/// The outcome of [`claim`].
#[derive(Debug)]
pub enum Claim {
    /// This process is the instance.
    Primary(Instance),
    /// Another process owns the name: [`forward`] the request to it and exit.
    Secondary(Bus),
}

/// Tries to become the instance named `name` on `bus`.
pub async fn claim(bus: Bus, name: &str) -> Result<Claim, DesktopError> {
    let path = object_path(name);
    let (tx, rx) = mpsc::unbounded_channel();
    bus.connection()
        .object_server()
        .at(path.as_str(), Application { tx })
        .await?;
    let reply = bus
        .connection()
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await;
    match reply {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Claim::Primary(Instance {
                bus,
                name: name.to_owned(),
                requests: Some(rx),
            }))
        }
        Err(e) if e != zbus::Error::NameTaken => Err(e.into()),
        // zbus reports `Exists` as `NameTaken`.
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(_) => {
            // Not ours: the object must not answer calls meant for the instance.
            let _ = bus
                .connection()
                .object_server()
                .remove::<Application, _>(path.as_str())
                .await;
            Ok(Claim::Secondary(bus))
        }
    }
}

#[zbus::proxy(interface = "org.freedesktop.Application")]
trait RemoteApplication {
    fn activate(&self, platform_data: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn open(&self, uris: &[&str], platform_data: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn activate_action(
        &self,
        action_name: &str,
        parameter: &[Value<'_>],
        platform_data: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// Sends `request` to the instance that owns `name`.
pub async fn forward(bus: &Bus, name: &str, request: &InstanceRequest) -> Result<(), DesktopError> {
    let path = object_path(name);
    let proxy = RemoteApplicationProxy::builder(bus.connection())
        .destination(name)?
        .path(path.as_str())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let data = |token: &Option<String>| {
        let mut m: HashMap<&str, Value<'_>> = HashMap::new();
        if let Some(t) = token {
            m.insert(TOKEN_KEY, Value::from(t.clone()));
        }
        m
    };
    match request {
        InstanceRequest::Activate { token } => proxy.activate(data(token)).await?,
        InstanceRequest::Open { uris, token } => {
            let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
            proxy.open(&uris, data(token)).await?;
        }
        InstanceRequest::Action { name, token } => {
            proxy.activate_action(name, &[], data(token)).await?;
        }
    }
    Ok(())
}
