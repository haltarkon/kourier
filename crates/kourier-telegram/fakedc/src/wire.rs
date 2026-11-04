//! The bytes of MTProto: TL strings and integers, the two hashes, and the MTProto 2.0 message key and
//! its key derivation (the spike's code, written from the protocol's description, 2026-10-16).

use grammers_crypto::aes::{ige_decrypt, ige_encrypt};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// The direction offset `x` of MTProto 2.0: 0 for client -> server, 8 for server -> client.
pub const FROM_CLIENT: usize = 0;
pub const FROM_SERVER: usize = 8;

pub fn sha1(d: &[u8]) -> [u8; 20] {
    let mut o = [0u8; 20];
    o.copy_from_slice(&Sha1::digest(d));
    o
}

pub fn sha256(d: &[u8]) -> [u8; 32] {
    let mut o = [0u8; 32];
    o.copy_from_slice(&Sha256::digest(d));
    o
}

/// A little-endian integer at `o`, or `None` past the end.
pub fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

pub fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    u32_at(b, o).map(|v| v as i32)
}

pub fn i64_at(b: &[u8], o: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// TL `string` / `bytes`: a one-byte length (or 254 and three bytes), the bytes, padding to 4.
pub fn tl_bytes(b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(b.len() + 8);
    if b.len() < 254 {
        v.push(b.len() as u8);
    } else {
        v.push(254);
        v.extend_from_slice(&(b.len() as u32).to_le_bytes()[..3]);
    }
    v.extend_from_slice(b);
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
    v
}

/// Reads a TL `bytes` at `*off` and moves past it (padding included); `None` past the end.
pub fn read_tl_bytes(buf: &[u8], off: &mut usize) -> Option<Vec<u8>> {
    let first = *buf.get(*off)?;
    let (len, head) = if first < 254 {
        (usize::from(first), 1)
    } else {
        let b = buf.get(*off + 1..*off + 4)?;
        (
            usize::from(b[0]) | usize::from(b[1]) << 8 | usize::from(b[2]) << 16,
            4,
        )
    };
    let s = buf.get(*off + head..*off + head + len)?.to_vec();
    *off += (head + len + 3) & !3;
    Some(s)
}

/// MTProto 2.0's key and iv for one message: `x` is [`FROM_CLIENT`] or [`FROM_SERVER`].
pub fn kdf(key: &[u8; 256], msg_key: &[u8; 16], x: usize) -> ([u8; 32], [u8; 32]) {
    let a = Sha256::new()
        .chain_update(msg_key)
        .chain_update(&key[x..x + 36])
        .finalize();
    let b = Sha256::new()
        .chain_update(&key[40 + x..40 + x + 36])
        .chain_update(msg_key)
        .finalize();
    let mut k = [0u8; 32];
    k[..8].copy_from_slice(&a[..8]);
    k[8..24].copy_from_slice(&b[8..24]);
    k[24..].copy_from_slice(&a[24..32]);
    let mut iv = [0u8; 32];
    iv[..8].copy_from_slice(&b[..8]);
    iv[8..24].copy_from_slice(&a[8..24]);
    iv[24..].copy_from_slice(&b[24..32]);
    (k, iv)
}

/// The message key of a plaintext sent in direction `x`: the middle 16 bytes of
/// sha256(auth_key[88 + x .. 120 + x] ++ plaintext).
pub fn msg_key(key: &[u8; 256], plain: &[u8], x: usize) -> [u8; 16] {
    let h = Sha256::new()
        .chain_update(&key[88 + x..120 + x])
        .chain_update(plain)
        .finalize();
    let mut o = [0u8; 16];
    o.copy_from_slice(&h[8..24]);
    o
}

/// One encrypted message as the server sends it: key id, message key, the IGE-encrypted plaintext.
pub fn seal(key: &[u8; 256], key_id: &[u8; 8], mut plain: Vec<u8>) -> Vec<u8> {
    let pad = 12 + (16 - (plain.len() + 12) % 16) % 16;
    plain.extend(std::iter::repeat_n(0xAAu8, pad));
    let mk = msg_key(key, &plain, FROM_SERVER);
    let (k, iv) = kdf(key, &mk, FROM_SERVER);
    ige_encrypt(&mut plain, &k, &iv);
    let mut frame = Vec::with_capacity(24 + plain.len());
    frame.extend_from_slice(key_id);
    frame.extend_from_slice(&mk);
    frame.extend_from_slice(&plain);
    frame
}

/// The plaintext of a client's encrypted message (after the key id), or `None` when its message key
/// does not match (a bad packet).
pub fn open(key: &[u8; 256], rest: &[u8]) -> Option<Vec<u8>> {
    let mk: [u8; 16] = rest.get(..16)?.try_into().ok()?;
    let mut plain = rest.get(16..)?.to_vec();
    if plain.is_empty() || !plain.len().is_multiple_of(16) {
        return None;
    }
    let (k, iv) = kdf(key, &mk, FROM_CLIENT);
    ige_decrypt(&mut plain, &k, &iv);
    (msg_key(key, &plain, FROM_CLIENT) == mk).then_some(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 256] {
        let mut k = [0u8; 256];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        k
    }

    /// The formula written out a second time, from the protocol's description (msg_key_large,
    /// sha256_a, sha256_b), to decrypt what [`seal`] wrote as a CLIENT would.
    fn client_open(key: &[u8; 256], frame: &[u8]) -> Vec<u8> {
        let mk: [u8; 16] = frame[8..24].try_into().unwrap();
        let x = 8;
        let sha_a = sha256(&[&mk[..], &key[x..x + 36]].concat());
        let sha_b = sha256(&[&key[40 + x..76 + x], &mk[..]].concat());
        let aes_key = [&sha_a[0..8], &sha_b[8..24], &sha_a[24..32]].concat();
        let aes_iv = [&sha_b[0..8], &sha_a[8..24], &sha_b[24..32]].concat();
        let mut p = frame[24..].to_vec();
        ige_decrypt(
            &mut p,
            &aes_key.try_into().unwrap(),
            &aes_iv.try_into().unwrap(),
        );
        let large = sha256(&[&key[88 + x..120 + x], &p[..]].concat());
        assert_eq!(
            &large[8..24],
            &mk[..],
            "the message key of a server message"
        );
        p
    }

    #[test]
    fn a_server_message_opens_with_the_server_to_client_formula() {
        let key = test_key();
        let plain: Vec<u8> = (0u8..40).collect();
        let frame = seal(&key, &[9; 8], plain.clone());
        assert_eq!(&frame[..8], &[9; 8]);
        let opened = client_open(&key, &frame);
        assert_eq!(&opened[..40], &plain[..]);
        assert!(opened.len() >= 40 + 12 && opened.len().is_multiple_of(16));
    }

    #[test]
    fn a_client_message_is_opened_and_a_changed_byte_is_refused() {
        let key = test_key();
        // Sealed as a client would (x = 0), with the same functions' formula.
        let mut plain: Vec<u8> = (0u8..48).collect();
        let mk = msg_key(&key, &plain, FROM_CLIENT);
        let (k, iv) = kdf(&key, &mk, FROM_CLIENT);
        ige_encrypt(&mut plain, &k, &iv);
        let mut rest = mk.to_vec();
        rest.extend_from_slice(&plain);
        assert_eq!(open(&key, &rest).unwrap(), (0u8..48).collect::<Vec<_>>());
        let last = rest.len() - 1;
        rest[last] ^= 1;
        assert!(open(&key, &rest).is_none());
    }

    #[test]
    fn tl_bytes_round_trip_short_and_long() {
        for len in [0usize, 1, 3, 253, 254, 300] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let enc = tl_bytes(&data);
            assert!(enc.len().is_multiple_of(4));
            let mut off = 0;
            assert_eq!(read_tl_bytes(&enc, &mut off).unwrap(), data);
            assert_eq!(off, enc.len());
        }
        let mut off = 0;
        assert!(read_tl_bytes(&[10, 1, 2], &mut off).is_none());
    }
}
