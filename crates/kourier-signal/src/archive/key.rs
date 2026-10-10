//! The key of a transfer archive.

use std::fmt;

use libsignal_account_keys::BackupKey;
use libsignal_core::Aci;
use libsignal_message_backup::key::MessageBackupKey;
use zeroize::Zeroize as _;

/// The HMAC key and the AES-256 key of one archive, derived by libsignal. Wiped on drop;
/// `Debug` prints nothing. Not `Clone`: every copy of a key is one more place to wipe.
pub(crate) struct ArchiveKey {
    keys: MessageBackupKey,
}

impl ArchiveKey {
    /// The key of the archive a phone uploads at link time (`backup5`): the 32 bytes of the
    /// provisioning message's `ephemeralBackupKey` as a libsignal `BackupKey`, the backup id
    /// derived from our ACI, and no forward-secrecy token. This is what the phone writes
    /// (`EncryptedBackupWriter.createForLocalOrLinking`) and Desktop reads
    /// (`ts/services/backups/index.preload.ts`).
    ///
    /// The intermediate `BackupKey` and `BackupId` are wiped here; what libsignal's AES and
    /// HMAC readers copy into their own state while an archive is read is theirs to wipe.
    pub(crate) fn for_transfer(ephemeral_key: &[u8; 32], own_aci: Aci) -> Self {
        Self::derive(ephemeral_key, own_aci, None)
    }

    fn derive(
        backup_key: &[u8; 32],
        own_aci: Aci,
        token: Option<&libsignal_account_keys::BackupForwardSecrecyToken>,
    ) -> Self {
        let mut backup_key: BackupKey = BackupKey(*backup_key);
        let mut backup_id = backup_key.derive_backup_id(&own_aci);
        let keys = MessageBackupKey::derive(&backup_key, &backup_id, token);
        backup_key.0.zeroize();
        backup_id.0.zeroize();
        Self { keys }
    }

    /// For libsignal's readers (this module's reader, and the validator in tests).
    pub(super) fn message_backup_key(&self) -> &MessageBackupKey {
        &self.keys
    }

    /// The 64 bytes `AES key ‖ HMAC key` of the writer (`attachment::AttachmentKeys` order).
    #[cfg(any(test, feature = "testing"))]
    pub(super) fn aes_then_hmac(&self) -> zeroize::Zeroizing<[u8; 64]> {
        let mut out = zeroize::Zeroizing::new([0u8; 64]);
        out[..32].copy_from_slice(&self.keys.aes_key);
        out[32..].copy_from_slice(&self.keys.hmac_key);
        out
    }
}

impl Drop for ArchiveKey {
    fn drop(&mut self) {
        self.keys.hmac_key.zeroize();
        self.keys.aes_key.zeroize();
    }
}

impl fmt::Debug for ArchiveKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveKey(<redacted>)")
    }
}
