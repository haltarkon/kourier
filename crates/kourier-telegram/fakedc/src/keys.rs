//! The fake DC's RSA keys (brief §3.2 "Keys", §3.4): `tg-fakedc keygen --out <dir>` makes TWO 2048-bit
//! keys with e = 65537. The TEST key's private half is written (0600) for the fake DC; the SECOND key
//! only ever has its public half written: it replaces TDLib's production key in the test-only library,
//! so that library cannot finish a handshake with any server, ours included.
//!
//! The public halves are PKCS#1 PEM blocks laid out exactly as TDLib's compiled keys are (64 columns,
//! no newline after the END line): [`PUBLIC_PEM_LEN`] bytes for every 2048-bit modulus, asserted, so
//! the test-only library's same-length byte patch always fits.

use crate::wire::{sha1, tl_bytes};
use num_bigint::BigUint;
use rand::RngCore;
use std::fmt;

/// The modulus size every key has, exactly.
pub const MODULUS_BITS: u64 = 2048;
pub const PUBLIC_EXPONENT: u32 = 65537;
/// The length of a 2048-bit key's PKCS#1 public PEM block, BEGIN line to END line.
pub const PUBLIC_PEM_LEN: usize = 425;

const PUBLIC_BEGIN: &str = "-----BEGIN RSA PUBLIC KEY-----\n";
const PUBLIC_END: &str = "-----END RSA PUBLIC KEY-----";
const PRIVATE_BEGIN: &str = "-----BEGIN RSA PRIVATE KEY-----\n";
const PRIVATE_END: &str = "-----END RSA PRIVATE KEY-----";

#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The modulus does not have exactly [`MODULUS_BITS`] bits.
    Bits(u64),
    /// The public PEM block does not have [`PUBLIC_PEM_LEN`] bytes.
    PemLength(usize),
    /// A PEM or DER that does not parse.
    Parse(&'static str),
    /// The written PEM does not read back as the same key.
    RoundTrip,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Bits(n) => write!(f, "the modulus has {n} bits, not {MODULUS_BITS}"),
            KeyError::PemLength(n) => {
                write!(f, "the public PEM has {n} bytes, not {PUBLIC_PEM_LEN}")
            }
            KeyError::Parse(what) => write!(f, "cannot parse {what}"),
            KeyError::RoundTrip => write!(f, "the PEM does not read back as the same key"),
        }
    }
}

/// A key the fake DC holds: the public modulus and, for the test key, the private exponent.
pub struct RsaKey {
    pub n: BigUint,
    pub e: BigUint,
    pub d: BigUint,
}

/// A freshly made key with its primes (only [`private_pem`] uses them).
pub struct NewKey {
    pub key: RsaKey,
    p: BigUint,
    q: BigUint,
}

impl RsaKey {
    /// TDLib's fingerprint of a public key: the low 8 bytes of sha1(TL bytes n ++ TL bytes e).
    pub fn fingerprint(&self) -> i64 {
        let mut b = tl_bytes(&self.n.to_bytes_be());
        b.extend(tl_bytes(&self.e.to_bytes_be()));
        let h = sha1(&b);
        i64::from_le_bytes(h[12..20].try_into().expect("8 bytes"))
    }

    pub fn public_pem(&self) -> String {
        let der = der_sequence(&[der_uint(&self.n), der_uint(&self.e)]);
        pem(PUBLIC_BEGIN, PUBLIC_END, &der)
    }
}

// ----- prime generation -----

const SMALL_PRIMES: [u32; 30] = [
    3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97,
    101, 103, 107, 109, 113, 127,
];

fn random_below(limit: &BigUint, rng: &mut impl RngCore) -> BigUint {
    let bytes = limit.to_bytes_be().len();
    loop {
        let mut b = vec![0u8; bytes];
        rng.fill_bytes(&mut b);
        let v = BigUint::from_bytes_be(&b);
        if &v < limit && v > BigUint::from(1u32) {
            return v;
        }
    }
}

/// Miller-Rabin with `rounds` random bases.
fn probably_prime(n: &BigUint, rounds: usize, rng: &mut impl RngCore) -> bool {
    let one = BigUint::from(1u32);
    let two = BigUint::from(2u32);
    if *n < two {
        return false;
    }
    for p in SMALL_PRIMES {
        let p = BigUint::from(p);
        if *n == p {
            return true;
        }
        if (n % &p) == BigUint::from(0u32) {
            return false;
        }
    }
    let n1 = n - &one;
    let s = n1.trailing_zeros().unwrap_or(0);
    let d = &n1 >> s;
    'bases: for _ in 0..rounds {
        let a = random_below(&n1, rng);
        let mut x = a.modpow(&d, n);
        if x == one || x == n1 {
            continue;
        }
        for _ in 1..s {
            x = x.modpow(&two, n);
            if x == n1 {
                continue 'bases;
            }
        }
        return false;
    }
    true
}

/// A 1024-bit prime with its two top bits set (so a product of two has exactly 2048 bits) and
/// p - 1 prime to e.
fn prime_1024(rng: &mut impl RngCore) -> BigUint {
    let e = BigUint::from(PUBLIC_EXPONENT);
    loop {
        let mut b = [0u8; 128];
        rng.fill_bytes(&mut b);
        b[0] |= 0xC0;
        b[127] |= 1;
        let p = BigUint::from_bytes_be(&b);
        if (&p - 1u32) % &e == BigUint::from(0u32) {
            continue;
        }
        if probably_prime(&p, 40, rng) {
            return p;
        }
    }
}

/// The key of two primes, with the checks every key must pass: exactly 2048 bits, the public PEM
/// exactly [`PUBLIC_PEM_LEN`] bytes, and both PEMs read back as this key.
pub fn key_from_primes(p: BigUint, q: BigUint) -> Result<NewKey, KeyError> {
    let n = &p * &q;
    if n.bits() != MODULUS_BITS {
        return Err(KeyError::Bits(n.bits()));
    }
    let e = BigUint::from(PUBLIC_EXPONENT);
    let phi = (&p - 1u32) * (&q - 1u32);
    let d = e.modinv(&phi).ok_or(KeyError::Parse("e has no inverse"))?;
    let new = NewKey {
        key: RsaKey { n, e, d },
        p,
        q,
    };
    let public = new.key.public_pem();
    if public.len() != PUBLIC_PEM_LEN {
        return Err(KeyError::PemLength(public.len()));
    }
    let back = parse_public_pem(&public)?;
    let private = parse_private_pem(&new.private_pem())?;
    if back.n != new.key.n
        || back.e != new.key.e
        || private.n != new.key.n
        || private.d != new.key.d
    {
        return Err(KeyError::RoundTrip);
    }
    Ok(new)
}

pub fn generate(rng: &mut impl RngCore) -> Result<NewKey, KeyError> {
    loop {
        let p = prime_1024(rng);
        let q = prime_1024(rng);
        if p != q {
            return key_from_primes(p, q);
        }
    }
}

impl NewKey {
    /// PKCS#1 `RSAPrivateKey`: version, n, e, d, p, q, d mod (p-1), d mod (q-1), q^-1 mod p.
    pub fn private_pem(&self) -> String {
        let k = &self.key;
        let dp = &k.d % (&self.p - 1u32);
        let dq = &k.d % (&self.q - 1u32);
        let qinv = self.q.modinv(&self.p).unwrap_or_default();
        let der = der_sequence(&[
            der_uint(&BigUint::from(0u32)),
            der_uint(&k.n),
            der_uint(&k.e),
            der_uint(&k.d),
            der_uint(&self.p),
            der_uint(&self.q),
            der_uint(&dp),
            der_uint(&dq),
            der_uint(&qinv),
        ]);
        let mut text = pem(PRIVATE_BEGIN, PRIVATE_END, &der);
        text.push('\n');
        text
    }
}

// ----- DER, base64, PEM -----

fn der_len(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = (len as u32)
            .to_be_bytes()
            .into_iter()
            .skip_while(|b| *b == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
}

fn der_uint(v: &BigUint) -> Vec<u8> {
    let mut body = v.to_bytes_be();
    if body[0] & 0x80 != 0 {
        body.insert(0, 0);
    }
    let mut out = vec![0x02];
    der_len(body.len(), &mut out);
    out.extend(body);
    out
}

fn der_sequence(items: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = items.concat();
    let mut out = vec![0x30];
    der_len(body.len(), &mut out);
    out.extend(body);
    out
}

/// One DER element at `*off`: (tag, contents), moving past it.
fn der_read<'a>(buf: &'a [u8], off: &mut usize) -> Result<(u8, &'a [u8]), KeyError> {
    let bad = KeyError::Parse("DER");
    let tag = *buf.get(*off).ok_or(KeyError::Parse("DER"))?;
    let first = *buf.get(*off + 1).ok_or(KeyError::Parse("DER"))?;
    let mut at = *off + 2;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return Err(bad);
        }
        let mut l = 0usize;
        for i in 0..n {
            l = (l << 8) | usize::from(*buf.get(at + i).ok_or(KeyError::Parse("DER"))?);
        }
        at += n;
        l
    };
    let body = buf.get(at..at + len).ok_or(bad)?;
    *off = at + len;
    Ok((tag, body))
}

/// The INTEGERs of a DER SEQUENCE.
fn der_integers(der: &[u8]) -> Result<Vec<BigUint>, KeyError> {
    let mut off = 0;
    let (tag, body) = der_read(der, &mut off)?;
    if tag != 0x30 || off != der.len() {
        return Err(KeyError::Parse("a DER sequence"));
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let (tag, v) = der_read(body, &mut at)?;
        if tag != 0x02 {
            return Err(KeyError::Parse("a DER integer"));
        }
        out.push(BigUint::from_bytes_be(v));
    }
    Ok(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let v = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((v >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> Result<Vec<u8>, KeyError> {
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in text
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        let v = B64
            .iter()
            .position(|&x| x == c)
            .ok_or(KeyError::Parse("base64"))? as u32;
        bits = bits << 6 | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    Ok(out)
}

fn pem(begin: &str, end: &str, der: &[u8]) -> String {
    let mut text = String::from(begin);
    let b = base64(der);
    for line in b.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        text.push('\n');
    }
    text.push_str(end);
    text
}

fn pem_body<'a>(text: &'a str, begin: &str, end: &str) -> Result<&'a str, KeyError> {
    let start = text
        .find(begin)
        .ok_or(KeyError::Parse("the PEM begin line"))?
        + begin.len();
    let stop = text[start..]
        .find(end)
        .ok_or(KeyError::Parse("the PEM end line"))?;
    Ok(&text[start..start + stop])
}

pub fn parse_public_pem(text: &str) -> Result<RsaKey, KeyError> {
    let der = unbase64(pem_body(text, PUBLIC_BEGIN, PUBLIC_END)?)?;
    match der_integers(&der)?.as_slice() {
        [n, e] => Ok(RsaKey {
            n: n.clone(),
            e: e.clone(),
            d: BigUint::default(),
        }),
        _ => Err(KeyError::Parse("an RSA public key")),
    }
}

pub fn parse_private_pem(text: &str) -> Result<RsaKey, KeyError> {
    let der = unbase64(pem_body(text, PRIVATE_BEGIN, PRIVATE_END)?)?;
    match der_integers(&der)?.as_slice() {
        [_version, n, e, d, ..] => {
            if n.bits() != MODULUS_BITS {
                return Err(KeyError::Bits(n.bits()));
            }
            Ok(RsaKey {
                n: n.clone(),
                e: e.clone(),
                d: d.clone(),
            })
        }
        _ => Err(KeyError::Parse("an RSA private key")),
    }
}

/// `keygen --out <dir>`: `test-key.pem` (private, 0600), `test-public.pem` and `second-public.pem`
/// (public halves, the PEM block and a newline) and `fingerprints.txt` (TDLib's fingerprint of each,
/// for the test-only library's manifest). The second key's private half is dropped unwritten.
pub fn keygen(out: &std::path::Path) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut rng = rand::thread_rng();
    let test = generate(&mut rng).map_err(|e| e.to_string())?;
    let second = generate(&mut rng).map_err(|e| e.to_string())?;
    if test.key.n == second.key.n {
        return Err("two equal keys".into());
    }
    std::fs::create_dir_all(out).map_err(|e| format!("create the key directory: {e}"))?;
    let write = |name: &str, text: &str, mode: u32| -> Result<(), String> {
        let path = out.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&path)
            .map_err(|e| format!("create {name}: {e}"))?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write {name}: {e}"))
    };
    write("test-key.pem", &test.private_pem(), 0o600)?;
    write(
        "test-public.pem",
        &format!("{}\n", test.key.public_pem()),
        0o644,
    )?;
    write(
        "second-public.pem",
        &format!("{}\n", second.key.public_pem()),
        0o644,
    )?;
    write(
        "fingerprints.txt",
        &format!(
            "test {:016x}\nsecond {:016x}\n",
            test.key.fingerprint() as u64,
            second.key.fingerprint() as u64
        ),
        0o644,
    )?;
    drop(second);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn a_new_key_has_2048_bits_and_a_pem_of_the_fixed_length() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let k = generate(&mut rng).unwrap();
        assert_eq!(k.key.n.bits(), MODULUS_BITS);
        let public = k.key.public_pem();
        assert_eq!(public.len(), PUBLIC_PEM_LEN);
        assert!(public.starts_with(PUBLIC_BEGIN) && public.ends_with(PUBLIC_END));
        let lines: Vec<usize> = public.lines().map(str::len).collect();
        assert_eq!(lines, [30, 64, 64, 64, 64, 64, 40, 28]);
        // RSA works with the pair.
        let m = BigUint::from(123_456_789u32);
        assert_eq!(m.modpow(&k.key.e, &k.key.n).modpow(&k.key.d, &k.key.n), m);
        let private = parse_private_pem(&k.private_pem()).unwrap();
        assert_eq!(private.fingerprint(), k.key.fingerprint());
    }

    #[test]
    fn a_modulus_of_another_size_is_refused() {
        // Two small primes: the bit check comes first and says so.
        let r = key_from_primes(BigUint::from(61u32), BigUint::from(53u32));
        assert_eq!(r.err(), Some(KeyError::Bits(12)));
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        let p = prime_1024(&mut rng);
        let small = {
            let mut b = [0u8; 127];
            loop {
                rng.fill_bytes(&mut b);
                b[0] |= 0x80;
                b[126] |= 1;
                let q = BigUint::from_bytes_be(&b);
                if probably_prime(&q, 20, &mut rng) {
                    break q;
                }
            }
        };
        let bits = (&p * &small).bits();
        assert!(bits < MODULUS_BITS);
        assert_eq!(key_from_primes(p, small).err(), Some(KeyError::Bits(bits)));
    }

    #[test]
    fn miller_rabin_tells_primes_from_composites() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        // 2^127 - 1 is prime; 561 is a Carmichael number.
        let m127 = (BigUint::from(1u32) << 127u32) - 1u32;
        assert!(probably_prime(&m127, 20, &mut rng));
        assert!(!probably_prime(&BigUint::from(561u32), 20, &mut rng));
        assert!(!probably_prime(&(&m127 * &m127), 20, &mut rng));
    }

    #[test]
    fn base64_round_trips() {
        for len in 0..10 {
            let data: Vec<u8> = (0..len).map(|i| (i * 51) as u8).collect();
            assert_eq!(unbase64(&base64(&data)).unwrap(), data);
        }
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
    }
}
