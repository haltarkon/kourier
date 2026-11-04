//! The real MTProto auth-key handshake on the server's side (the spike's, 2026-10-16): `req_pq_multi`
//! -> `resPQ`, `req_DH_params` (RSA_PAD undone with the test key) -> `server_DH_params_ok`,
//! `set_client_DH_params` -> `dh_gen_ok`. TDLib cannot skip it: it makes its own auth key.

use crate::keys::RsaKey;
use crate::wire::{i64_at, read_tl_bytes, sha1, sha256, tl_bytes, u32_at};
use grammers_crypto::aes::{ige_decrypt, ige_encrypt};
use num_bigint::BigUint;
use rand::RngCore;

/// RFC 3526's 2048-bit MODP group (group 14), a public constant; g = 2.
pub const DH_PRIME: [u8; 256] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xC9, 0x0F, 0xDA, 0xA2, 0x21, 0x68, 0xC2, 0x34,
    0xC4, 0xC6, 0x62, 0x8B, 0x80, 0xDC, 0x1C, 0xD1, 0x29, 0x02, 0x4E, 0x08, 0x8A, 0x67, 0xCC, 0x74,
    0x02, 0x0B, 0xBE, 0xA6, 0x3B, 0x13, 0x9B, 0x22, 0x51, 0x4A, 0x08, 0x79, 0x8E, 0x34, 0x04, 0xDD,
    0xEF, 0x95, 0x19, 0xB3, 0xCD, 0x3A, 0x43, 0x1B, 0x30, 0x2B, 0x0A, 0x6D, 0xF2, 0x5F, 0x14, 0x37,
    0x4F, 0xE1, 0x35, 0x6D, 0x6D, 0x51, 0xC2, 0x45, 0xE4, 0x85, 0xB5, 0x76, 0x62, 0x5E, 0x7E, 0xC6,
    0xF4, 0x4C, 0x42, 0xE9, 0xA6, 0x37, 0xED, 0x6B, 0x0B, 0xFF, 0x5C, 0xB6, 0xF4, 0x06, 0xB7, 0xED,
    0xEE, 0x38, 0x6B, 0xFB, 0x5A, 0x89, 0x9F, 0xA5, 0xAE, 0x9F, 0x24, 0x11, 0x7C, 0x4B, 0x1F, 0xE6,
    0x49, 0x28, 0x66, 0x51, 0xEC, 0xE4, 0x5B, 0x3D, 0xC2, 0x00, 0x7C, 0xB8, 0xA1, 0x63, 0xBF, 0x05,
    0x98, 0xDA, 0x48, 0x36, 0x1C, 0x55, 0xD3, 0x9A, 0x69, 0x16, 0x3F, 0xA8, 0xFD, 0x24, 0xCF, 0x5F,
    0x83, 0x65, 0x5D, 0x23, 0xDC, 0xA3, 0xAD, 0x96, 0x1C, 0x62, 0xF3, 0x56, 0x20, 0x85, 0x52, 0xBB,
    0x9E, 0xD5, 0x29, 0x07, 0x70, 0x96, 0x96, 0x6D, 0x67, 0x0C, 0x35, 0x4E, 0x4A, 0xBC, 0x98, 0x04,
    0xF1, 0x74, 0x6C, 0x08, 0xCA, 0x18, 0x21, 0x7C, 0x32, 0x90, 0x5E, 0x46, 0x2E, 0x36, 0xCE, 0x3B,
    0xE3, 0x9E, 0x77, 0x2C, 0x18, 0x0E, 0x86, 0x03, 0x9B, 0x27, 0x83, 0xA2, 0xEC, 0x07, 0xA2, 0x8F,
    0xB5, 0xC5, 0x5D, 0xF0, 0x6F, 0x4C, 0x52, 0xC9, 0xDE, 0x2B, 0xCB, 0xF6, 0x95, 0x58, 0x17, 0x18,
    0x39, 0x95, 0x49, 0x7C, 0xEA, 0x95, 0x6A, 0xE5, 0x15, 0xD2, 0x26, 0x18, 0x98, 0xFA, 0x05, 0x10,
    0x15, 0x72, 0x8E, 0x5A, 0x8A, 0xAC, 0xAA, 0x68, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];

// Constructors of the handshake (layer-independent MTProto).
pub const REQ_PQ_MULTI: u32 = 0xbe7e8ef1;
pub const REQ_DH_PARAMS: u32 = 0xd712e4be;
pub const SET_CLIENT_DH_PARAMS: u32 = 0xf5045f1f;
const RES_PQ: u32 = 0x05162463;
const VECTOR: u32 = 0x1cb5c415;
const P_Q_INNER_DATA_DC: u32 = 0xa9f55f95;
const P_Q_INNER_DATA_TEMP_DC: u32 = 0x56fddf88;
const SERVER_DH_INNER_DATA: u32 = 0xb5890dba;
const SERVER_DH_PARAMS_OK: u32 = 0xd0e8075c;
const CLIENT_DH_INNER_DATA: u32 = 0x6643b654;
const DH_GEN_OK: u32 = 0x3bcbf734;

/// A fixed pq the client factors (it is checked, not secret).
const PQ: u64 = 0x17ED48941A08F981;

pub fn dh_prime() -> BigUint {
    BigUint::from_bytes_be(&DH_PRIME)
}

/// An auth key the handshake established: the 256 key bytes, its id and the first server salt.
#[derive(Clone, Copy)]
pub struct AuthKey {
    pub key: [u8; 256],
    pub id: [u8; 8],
    pub salt: i64,
}

/// The state between the handshake's three steps on one connection.
#[derive(Default)]
pub struct Handshake {
    nonce: [u8; 16],
    server_nonce: [u8; 16],
    new_nonce: [u8; 32],
    a: Option<BigUint>,
    tmp_key: [u8; 32],
    tmp_iv: [u8; 32],
}

/// What a handshake step answers.
pub enum Step {
    Reply(Vec<u8>),
    /// The key is made; the reply is `dh_gen_ok`.
    Done(Box<AuthKey>, Vec<u8>),
}

fn rand_array<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// Undoes RSA_PAD: decrypts with the private key, removes the temporary AES key's xor and the IGE
/// layer, and checks the hash. Returns the 192 padded data bytes (in their original order).
pub fn rsa_pad_open(key: &RsaKey, encrypted: &[u8]) -> Result<Vec<u8>, &'static str> {
    let c = BigUint::from_bytes_be(encrypted);
    if c >= key.n {
        return Err("RSA_PAD: the ciphertext is not below the modulus");
    }
    let m = c.modpow(&key.d, &key.n).to_bytes_be();
    if m.len() > 256 {
        return Err("RSA_PAD: too long");
    }
    let mut padded = vec![0u8; 256 - m.len()];
    padded.extend(m);
    let aes_encrypted = &padded[32..];
    let hash = sha256(aes_encrypted);
    let mut temp_key = [0u8; 32];
    for i in 0..32 {
        temp_key[i] = padded[i] ^ hash[i];
    }
    let mut data_with_hash = aes_encrypted.to_vec();
    ige_decrypt(&mut data_with_hash, &temp_key, &[0u8; 32]);
    let mut data = data_with_hash[..192].to_vec();
    data.reverse();
    let check = sha256(&[&temp_key[..], &data[..]].concat());
    if check[..] != data_with_hash[192..224] {
        return Err("RSA_PAD: the hash does not match");
    }
    Ok(data)
}

impl Handshake {
    /// One unencrypted handshake message (`body` = constructor and fields).
    pub fn step(&mut self, rsa: &RsaKey, body: &[u8]) -> Result<Step, String> {
        let short = || "a handshake message too short".to_string();
        match u32_at(body, 0).ok_or_else(short)? {
            REQ_PQ_MULTI => {
                self.nonce = body.get(4..20).ok_or_else(short)?.try_into().expect("16");
                self.server_nonce = rand_array();
                self.a = Some(BigUint::from_bytes_be(&rand_array::<32>()));
                let mut r = RES_PQ.to_le_bytes().to_vec();
                r.extend(self.nonce);
                r.extend(self.server_nonce);
                r.extend(tl_bytes(&PQ.to_be_bytes()));
                r.extend(VECTOR.to_le_bytes());
                r.extend(1i32.to_le_bytes());
                r.extend(rsa.fingerprint().to_le_bytes());
                Ok(Step::Reply(r))
            }
            REQ_DH_PARAMS => {
                let a = self.a.clone().ok_or("req_DH_params before req_pq_multi")?;
                let mut off = 36;
                read_tl_bytes(body, &mut off).ok_or_else(short)?;
                read_tl_bytes(body, &mut off).ok_or_else(short)?;
                let fp = i64_at(body, off).ok_or_else(short)?;
                off += 8;
                if fp != rsa.fingerprint() {
                    return Err("req_DH_params names another key".into());
                }
                let encrypted = read_tl_bytes(body, &mut off).ok_or_else(short)?;
                let data = rsa_pad_open(rsa, &encrypted)?;
                let ctor = u32_at(&data, 0).ok_or_else(short)?;
                if ctor != P_Q_INNER_DATA_DC && ctor != P_Q_INNER_DATA_TEMP_DC {
                    return Err("p_q_inner_data of an unknown kind".into());
                }
                let mut o = 4;
                for _ in 0..3 {
                    read_tl_bytes(&data, &mut o).ok_or_else(short)?;
                }
                let nonce: [u8; 16] = data
                    .get(o..o + 16)
                    .ok_or_else(short)?
                    .try_into()
                    .expect("16");
                let server_nonce: [u8; 16] = data
                    .get(o + 16..o + 32)
                    .ok_or_else(short)?
                    .try_into()
                    .expect("16");
                if nonce != self.nonce || server_nonce != self.server_nonce {
                    return Err("p_q_inner_data with other nonces".into());
                }
                self.new_nonce = data
                    .get(o + 32..o + 64)
                    .ok_or_else(short)?
                    .try_into()
                    .expect("32");
                let (nn, sn) = (self.new_nonce, self.server_nonce);
                let a1 = sha1(&[&nn[..], &sn[..]].concat());
                let a2 = sha1(&[&sn[..], &nn[..]].concat());
                let a3 = sha1(&[&nn[..], &nn[..]].concat());
                self.tmp_key[..20].copy_from_slice(&a1);
                self.tmp_key[20..].copy_from_slice(&a2[..12]);
                self.tmp_iv[..8].copy_from_slice(&a2[12..20]);
                self.tmp_iv[8..28].copy_from_slice(&a3);
                self.tmp_iv[28..].copy_from_slice(&nn[..4]);
                let p = dh_prime();
                let g_a = BigUint::from(2u32).modpow(&a, &p);
                let mut inner = SERVER_DH_INNER_DATA.to_le_bytes().to_vec();
                inner.extend(self.nonce);
                inner.extend(self.server_nonce);
                inner.extend(2i32.to_le_bytes());
                inner.extend(tl_bytes(&p.to_bytes_be()));
                inner.extend(tl_bytes(&g_a.to_bytes_be()));
                inner.extend((crate::now_secs() as i32).to_le_bytes());
                let mut answer = sha1(&inner).to_vec();
                answer.extend(&inner);
                while !answer.len().is_multiple_of(16) {
                    answer.push(0x55);
                }
                ige_encrypt(&mut answer, &self.tmp_key, &self.tmp_iv);
                let mut r = SERVER_DH_PARAMS_OK.to_le_bytes().to_vec();
                r.extend(self.nonce);
                r.extend(self.server_nonce);
                r.extend(tl_bytes(&answer));
                Ok(Step::Reply(r))
            }
            SET_CLIENT_DH_PARAMS => {
                let a = self
                    .a
                    .clone()
                    .ok_or("set_client_DH_params before req_pq_multi")?;
                let mut off = 36;
                let mut enc = read_tl_bytes(body, &mut off).ok_or_else(short)?;
                if !enc.len().is_multiple_of(16) || enc.len() < 32 {
                    return Err("client_DH_inner_data of a bad length".into());
                }
                ige_decrypt(&mut enc, &self.tmp_key, &self.tmp_iv);
                let inner = &enc[20..];
                if u32_at(inner, 0) != Some(CLIENT_DH_INNER_DATA) {
                    return Err("not client_DH_inner_data".into());
                }
                let mut o = 4 + 16 + 16 + 8;
                let g_b = BigUint::from_bytes_be(&read_tl_bytes(inner, &mut o).ok_or_else(short)?);
                let shared = g_b.modpow(&a, &dh_prime()).to_bytes_be();
                let mut key = [0u8; 256];
                key[256 - shared.len()..].copy_from_slice(&shared);
                let key_sha = sha1(&key);
                let id: [u8; 8] = key_sha[12..20].try_into().expect("8");
                let salt = i64_at(&self.new_nonce, 0).unwrap_or(0)
                    ^ i64_at(&self.server_nonce, 0).unwrap_or(0);
                let mut nh = self.new_nonce.to_vec();
                nh.push(1);
                nh.extend(&key_sha[..8]);
                let mut r = DH_GEN_OK.to_le_bytes().to_vec();
                r.extend(self.nonce);
                r.extend(self.server_nonce);
                r.extend(&sha1(&nh)[4..]);
                Ok(Step::Done(Box::new(AuthKey { key, id, salt }), r))
            }
            _ => Err("an unexpected unencrypted message".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys;
    use rand::SeedableRng;

    /// RSA_PAD as a client makes it (the protocol's description), with the hash given by the caller.
    fn client_rsa_pad(key: &RsaKey, data: &[u8], break_hash: bool) -> Vec<u8> {
        let mut rng = rand::rngs::StdRng::seed_from_u64(5);
        loop {
            let mut data_pad = data.to_vec();
            while data_pad.len() < 192 {
                data_pad.push(rng.next_u32() as u8);
            }
            let mut temp_key = [0u8; 32];
            rng.fill_bytes(&mut temp_key);
            let mut reversed = data_pad.clone();
            reversed.reverse();
            let mut hash = sha256(&[&temp_key[..], &data_pad[..]].concat());
            if break_hash {
                hash[0] ^= 1;
            }
            let mut with_hash = [reversed, hash.to_vec()].concat();
            ige_encrypt(&mut with_hash, &temp_key, &[0u8; 32]);
            let h = sha256(&with_hash);
            let mut key_aes = Vec::new();
            for i in 0..32 {
                key_aes.push(temp_key[i] ^ h[i]);
            }
            key_aes.extend(&with_hash);
            let m = BigUint::from_bytes_be(&key_aes);
            if m < key.n {
                return m.modpow(&key.e, &key.n).to_bytes_be();
            }
        }
    }

    #[test]
    fn rsa_pad_opens_what_a_client_sealed_and_refuses_a_wrong_hash() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        let k = keys::generate(&mut rng).unwrap().key;
        let data: Vec<u8> = (0u8..144).collect();
        let opened = rsa_pad_open(&k, &client_rsa_pad(&k, &data, false)).unwrap();
        assert_eq!(&opened[..144], &data[..]);
        assert_eq!(
            rsa_pad_open(&k, &client_rsa_pad(&k, &data, true)),
            Err("RSA_PAD: the hash does not match")
        );
    }

    #[test]
    fn the_dh_prime_is_rfc_3526_group_14() {
        let p = dh_prime();
        assert_eq!(p.bits(), 2048);
        // p mod 8 == 7, so g = 2 generates the subgroup TDLib checks for.
        assert_eq!(&p % 8u32, BigUint::from(7u32));
        assert_eq!(&DH_PRIME[..8], &[0xFF; 8]);
        assert_eq!(&DH_PRIME[248..], &[0xFF; 8]);
    }
}
