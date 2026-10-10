//! AES-256-GCM with a 12-byte IV in front and the 16-byte tag at the end: the cipher of profile
//! fields and avatars (key = the profile key) and of Storage Service manifests and records
//! (`SD/ts/Crypto.node.ts` `encryptProfile`/`decryptProfile`, used by both).

use signal_crypto::Aes256GcmDecryption;
use zeroize::Zeroizing;

/// IV length.
pub(crate) const GCM_IV_LEN: usize = 12;
/// Tag length.
pub(crate) const GCM_TAG_LEN: usize = 16;

/// Decryption failed: wrong key (a changed profile key), tampering or a truncated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("AES-GCM value did not decrypt")]
pub(crate) struct GcmError;

/// Decrypts `IV ‖ ciphertext ‖ tag` under `key`; the plaintext is zeroized on drop.
pub(crate) fn decrypt_gcm(key: &[u8; 32], data: &[u8]) -> Result<Zeroizing<Vec<u8>>, GcmError> {
    if data.len() < GCM_IV_LEN + GCM_TAG_LEN {
        return Err(GcmError);
    }
    let (iv, rest) = data.split_at(GCM_IV_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - GCM_TAG_LEN);
    let mut plain = Zeroizing::new(ciphertext.to_vec());
    let mut cipher = Aes256GcmDecryption::new(key, iv, &[]).map_err(|_| GcmError)?;
    cipher.decrypt(&mut plain);
    cipher.verify_tag(tag).map_err(|_| GcmError)?;
    Ok(plain)
}

/// Encrypts like Desktop's `encryptProfile` (tests: the harness and the phone encrypt in
/// production).
#[cfg(test)]
pub(crate) fn encrypt_gcm(key: &[u8; 32], iv: [u8; GCM_IV_LEN], plaintext: &[u8]) -> Vec<u8> {
    let mut buf = plaintext.to_vec();
    let mut cipher = signal_crypto::Aes256GcmEncryption::new(key, &iv, &[]).expect("cipher");
    cipher.encrypt(&mut buf);
    let tag = cipher.compute_tag();
    let mut out = iv.to_vec();
    out.extend_from_slice(&buf);
    out.extend_from_slice(&tag);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses_tampering_and_other_keys() {
        let key = [7u8; 32];
        let sealed = encrypt_gcm(&key, [1; 12], b"Ada\0Lindqvist");
        assert_eq!(sealed.len(), 12 + 13 + 16);
        assert_eq!(&decrypt_gcm(&key, &sealed).unwrap()[..], b"Ada\0Lindqvist");
        assert_eq!(decrypt_gcm(&[8; 32], &sealed), Err(GcmError));
        let mut tampered = sealed.clone();
        tampered[14] ^= 1;
        assert_eq!(decrypt_gcm(&key, &tampered), Err(GcmError));
        assert_eq!(decrypt_gcm(&key, &sealed[..27]), Err(GcmError));
        assert_eq!(
            &decrypt_gcm(&key, &encrypt_gcm(&key, [2; 12], b"")).unwrap()[..],
            b""
        );
    }
}
