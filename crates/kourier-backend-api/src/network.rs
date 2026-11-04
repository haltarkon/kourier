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

    /// The network-local id and `sent_at` of a new own message in `conv`. Not necessarily
    /// pure: the core calls it once per send, refuses an id that is taken (account-wide under
    /// `MsgIdScope::AccountAuthor`, in the conversation otherwise) and calls again, up to 1,000
    /// times, and persists the result in the
    /// outbox before the first transmit, so retries reuse it; a random id is fine (Threema's
    /// are). `now_ms` is strictly increasing per account (the core ensures it). `own` are the
    /// account's own ids; they never become the stored author. A network whose server gives the
    /// message an id of its own (`TransmitOutcome::SentAs`) allocates ids that can never equal
    /// one its server gives: the core takes an own row under the server's id for the message's
    /// echo.
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
    /// upload (a digest) cannot be given here. Default: none. A network whose server gives the
    /// message an id of its own (`TransmitOutcome::SentAs`) states none: a name made from the
    /// allocated id would be stale after the answer, and the core re-keys no attachment.
    fn attachment_net_ids(&self, local: &str, index: usize) -> Vec<String> {
        let _ = (local, index);
        Vec::new()
    }

    /// Whether the network has links to a person (P5b "Copy Link"): `false` (the default)
    /// hides the button on every page of this network.
    fn contact_links(&self) -> bool {
        false
    }

    /// The link to `user` made of `number`, a phone number the page MAY show (the core asks
    /// only then: `number_shown == Some(true)`, or the id is the number). `None` when the number
    /// does not make one (Signal: `https://signal.me/#p/<+E164>`, E.164 only, see [`is_e164`]).
    /// Never a link of Kourier's own. The result holds a third party's number: it must never
    /// reach a log. Default: none.
    fn contact_link(&self, user: &UserId, number: &str) -> Option<String> {
        let _ = (user, number);
        None
    }

    /// Starts the account actor (Signal: own OS thread + current-thread runtime + `LocalSet`,
    /// see [`crate::spawn_account_thread`]).
    fn spawn(&self, ctx: AccountContext) -> Result<AccountHandle, BackendError>;
}

/// A phone number in E.164 form: `+`, then 7 to 15 ASCII digits, the first not `0`
/// (what Signal's `signal.me/#p/` link accepts; Android's `SignalMeUtil` reads `\+[0-9]+`). The
/// lower bound is the log scrubber's (`kourier-app` `logging.rs`: 7 to 15 digits are a phone
/// number): Niue (+683) and Tokelau (+690) have numbers of 7 digits in all.
pub fn is_e164(number: &str) -> bool {
    let Some(digits) = number.strip_prefix('+') else {
        return false;
    };
    (7..=15).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0')
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
