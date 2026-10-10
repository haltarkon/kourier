//! The per-network factory ([`Network`]) and the registry of compiled-in networks
//! (architecture §4.2).

use std::fmt;
use std::sync::Arc;

use kourier_model::{BackendError, ConvKey, NetworkDescriptor, NetworkKind, UserId};

use crate::context::{AccountContext, AccountHandle};

/// The id of an own message, allocated before its first transmit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AllocatedId {
    /// Network-local part (the stored author of own messages is always `None`).
    pub local: Arc<str>,
    /// The sent timestamp that goes with it (ms).
    pub sent_at: i64,
}

/// One per network, process-wide, synchronous and dyn-compatible. Everything asynchronous
/// happens inside the account actor that [`Network::spawn`] starts.
pub trait Network: Send + Sync + 'static {
    /// Which network.
    fn kind(&self) -> NetworkKind;

    /// Static capabilities (§3.8 tier 1).
    fn descriptor(&self) -> &NetworkDescriptor;

    /// Pure function: the network-local id and `sent_at` of a new own message in `conv`. The
    /// core persists the result in the outbox before the first transmit, so retries reuse it;
    /// `now_ms` is strictly increasing per account (the core ensures it). `own` are the
    /// account's own ids; they never become the stored author.
    fn allocate_message_id(&self, own: &[UserId], conv: &ConvKey, now_ms: u64) -> AllocatedId;

    /// Pure function: the names the network will have for attachment number `index`
    /// (0-based, in the order of the message's `Body::Media`) of the own message `local`,
    /// as far as they are known before the send; the same strings the backend puts into
    /// `Attachment::net_ids` of attachments it emits and into
    /// `BackendEvent::AttachmentDeleted` (Signal: `uuid:<hex>` of the client UUID it derives
    /// from the message id and the index). The core stores them with the queued message
    /// (and hands them back in the request's `Attachment::net_ids`), so a delete of one
    /// attachment made on another own device finds the attachment of a message this device
    /// sent, and `Command::DeleteAttachment` can name it. Names that exist only after the
    /// upload (a digest) cannot be given here. Default: none.
    fn attachment_net_ids(&self, local: &str, index: usize) -> Vec<String> {
        let _ = (local, index);
        Vec::new()
    }

    /// Starts the account actor (Signal: own OS thread + current-thread runtime + `LocalSet`,
    /// see [`crate::spawn_account_thread`]).
    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError>;
}

/// Why a network could not be registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A network of this kind is registered already.
    #[error("network {0} registered twice")]
    Duplicate(NetworkKind),
}

/// The compiled-in networks, built by `kourier-app` from cargo features and injected into the
/// core (the core never names a backend). At most one network per [`NetworkKind`].
#[derive(Clone, Default)]
pub struct NetworkRegistry(Vec<Arc<dyn Network>>);

impl NetworkRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a network.
    pub fn register(&mut self, network: Arc<dyn Network>) -> Result<(), RegistryError> {
        let kind = network.kind();
        if self.get(kind).is_some() {
            return Err(RegistryError::Duplicate(kind));
        }
        self.0.push(network);
        Ok(())
    }

    /// Builder form of [`NetworkRegistry::register`].
    pub fn with(mut self, network: Arc<dyn Network>) -> Result<Self, RegistryError> {
        self.register(network)?;
        Ok(self)
    }

    /// The network of `kind`, if compiled in.
    pub fn get(&self, kind: NetworkKind) -> Option<&Arc<dyn Network>> {
        self.0.iter().find(|n| n.kind() == kind)
    }

    /// All networks, in registration order (the "add account" list).
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Network>> {
        self.0.iter()
    }

    /// Their kinds, in registration order.
    pub fn kinds(&self) -> impl Iterator<Item = NetworkKind> + '_ {
        self.0.iter().map(|n| n.kind())
    }

    /// Number of networks.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No network compiled in.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for NetworkRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.kinds()).finish()
    }
}
