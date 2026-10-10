//! A thin Secret Service client (`org.freedesktop.secrets`, through oo7).
//!
//! Kourier stores two kinds of items (§5.2): the master key and one secret per account. Which
//! attributes they carry is the caller's business (`kourier-app`); this module only stores,
//! finds and deletes 32-byte-ish blobs in the default collection. With `prompt = false` nothing
//! here ever shows a dialog: a locked collection is reported as [`SecretError::Locked`] and the
//! caller decides whether to ask the user (or fall back to passphrase mode).

use oo7::dbus::{Collection, Service};
use std::fmt;
use zeroize::Zeroizing;

/// Why a Secret Service call failed.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// No session bus, or no `org.freedesktop.secrets` service on it.
    #[error("no Secret Service: {0}")]
    Unavailable(String),
    /// The default collection is locked and prompting was not allowed.
    #[error("the Secret Service collection is locked")]
    Locked,
    /// The user dismissed the unlock or create prompt.
    #[error("the Secret Service prompt was dismissed")]
    Dismissed,
    /// There is no default collection and creating one was not allowed (it prompts).
    #[error("the Secret Service has no default collection")]
    NoDefaultCollection,
    /// Any other service error.
    #[error("Secret Service error: {0}")]
    Backend(String),
}

impl From<oo7::dbus::Error> for SecretError {
    fn from(e: oo7::dbus::Error) -> Self {
        use oo7::dbus::{Error as E, ServiceError};
        match e {
            E::ZBus(e) => SecretError::Unavailable(e.to_string()),
            E::Service(ServiceError::IsLocked(_)) => SecretError::Locked,
            E::Dismissed => SecretError::Dismissed,
            E::NotFound(_) => SecretError::NoDefaultCollection,
            other => SecretError::Backend(other.to_string()),
        }
    }
}

/// A connection to the Secret Service and its default collection.
pub struct SecretService {
    // Keeps the session (and its D-Bus connection) alive for `collection`.
    _service: Service,
    collection: Collection,
    prompt: bool,
}

impl fmt::Debug for SecretService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretService")
            .field("prompt", &self.prompt)
            .finish_non_exhaustive()
    }
}

impl SecretService {
    /// Connects over the session bus (an encrypted session when the service supports it) and
    /// opens the default collection. With `prompt`, a missing default collection is created and
    /// a locked one is unlocked later, both through the service's own dialog.
    pub async fn connect(prompt: bool) -> Result<Self, SecretError> {
        let service = Service::new().await?;
        let collection = match service.with_alias(Service::DEFAULT_COLLECTION).await? {
            Some(c) => c,
            None if prompt => service.default_collection().await?,
            None => return Err(SecretError::NoDefaultCollection),
        };
        Ok(Self {
            _service: service,
            collection,
            prompt,
        })
    }

    /// Whether the default collection is locked.
    pub async fn is_locked(&self) -> Result<bool, SecretError> {
        Ok(self.collection.is_locked().await?)
    }

    /// Finds the secret of the item matching `attributes` (the newest one when several do).
    pub async fn lookup(
        &self,
        attributes: &[(&str, &str)],
    ) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
        self.ensure_unlocked().await?;
        let mut items = self.collection.search_items(&attributes).await?;
        if items.len() > 1 {
            let mut dated = Vec::with_capacity(items.len());
            for item in items {
                dated.push((item.created().await?, item));
            }
            dated.sort_by_key(|(created, _)| *created);
            items = dated.into_iter().map(|(_, item)| item).collect();
        }
        let Some(item) = items.pop() else {
            return Ok(None);
        };
        if item.is_locked().await? {
            if !self.prompt {
                return Err(SecretError::Locked);
            }
            item.unlock(None).await?;
        }
        let secret = item.secret().await?;
        Ok(Some(Zeroizing::new(secret.as_bytes().to_vec())))
    }

    /// Stores `secret` under `attributes`, replacing an item with the same attributes.
    pub async fn store(
        &self,
        label: &str,
        attributes: &[(&str, &str)],
        secret: &[u8],
    ) -> Result<(), SecretError> {
        self.ensure_unlocked().await?;
        self.collection
            .create_item(label, &attributes, oo7::Secret::blob(secret), true, None)
            .await?;
        Ok(())
    }

    /// Deletes every item matching `attributes`; returns how many.
    pub async fn delete(&self, attributes: &[(&str, &str)]) -> Result<usize, SecretError> {
        self.ensure_unlocked().await?;
        let items = self.collection.search_items(&attributes).await?;
        let n = items.len();
        for item in items {
            item.delete(None).await?;
        }
        Ok(n)
    }

    async fn ensure_unlocked(&self) -> Result<(), SecretError> {
        if self.collection.is_locked().await? {
            if !self.prompt {
                return Err(SecretError::Locked);
            }
            self.collection.unlock(None).await?;
        }
        Ok(())
    }
}
