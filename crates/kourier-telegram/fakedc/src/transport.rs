//! What comes before MTProto on a connection: the test dialer's destination line (`KFDC1 <ip> <port>`)
//! and the obfuscated2 header of TDLib's transport (intermediate framing, no padding).

use aes::Aes256;
use ctr::Ctr128BE;
use ctr::cipher::{KeyIvInit, StreamCipher};
use std::net::{IpAddr, SocketAddr};

pub type Ctr = Ctr128BE<Aes256>;

/// The longest destination line the fake DC reads (`KFDC1 ` + an IPv6 literal + a port + `\n`).
pub const PREAMBLE_MAX: usize = 64;

/// The destination the remap dialer was asked for, from its line `KFDC1 <ip> <port>\n` (without the
/// newline). Anything else is refused.
pub fn parse_preamble(line: &[u8]) -> Option<SocketAddr> {
    let text = std::str::from_utf8(line).ok()?;
    let mut parts = text.split(' ');
    if parts.next()? != "KFDC1" {
        return None;
    }
    let ip: IpAddr = parts.next()?.parse().ok()?;
    let port: u16 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || port == 0 {
        return None;
    }
    Some(SocketAddr::new(ip, port))
}

/// The server's view of a connection's obfuscated2 header.
pub struct Header {
    /// Decrypts what the client sends (already advanced past the 64 header bytes).
    pub from_client: Ctr,
    /// Encrypts what the server sends.
    pub to_client: Ctr,
    /// The protocol tag (0xeeeeeeee for intermediate framing).
    pub tag: u32,
    /// The header's dc field: the DC the client thinks it dialled (test DCs carry 10000 + id,
    /// media DCs are negative).
    pub dc_field: i16,
}

impl Header {
    /// Reads the 64 bytes the client sent first.
    pub fn read(hdr: &[u8; 64]) -> Header {
        let mut from_client = Ctr::new((&hdr[8..40]).into(), (&hdr[40..56]).into());
        let mut plain = *hdr;
        from_client.apply_keystream(&mut plain);
        let tag = u32::from_le_bytes(plain[56..60].try_into().expect("4 bytes"));
        let dc_field = i16::from_le_bytes([plain[60], plain[61]]);
        let mut rev = *hdr;
        rev.reverse();
        let to_client = Ctr::new((&rev[8..40]).into(), (&rev[40..56]).into());
        Header {
            from_client,
            to_client,
            tag,
            dc_field,
        }
    }

    /// The DC number the fake answers as (`this_dc`): the dc field without its test and media marks.
    pub fn dc_id(&self) -> i32 {
        (i32::from(self.dc_field).abs() % 10000).max(1)
    }
}

/// One intermediate frame as the server writes it (length, payload), encrypted by `to_client`.
pub fn frame(to_client: &mut Ctr, payload: &[u8]) -> Vec<u8> {
    let mut f = (payload.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(payload);
    to_client.apply_keystream(&mut f);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preamble_names_one_address_and_port() {
        assert_eq!(
            parse_preamble(b"KFDC1 192.0.2.7 443"),
            Some("192.0.2.7:443".parse().unwrap())
        );
        assert_eq!(
            parse_preamble(b"KFDC1 2001:db8::1 80"),
            Some("[2001:db8::1]:80".parse().unwrap())
        );
        for bad in [
            &b"KFDC2 192.0.2.7 443"[..],
            b"KFDC1 192.0.2.7",
            b"KFDC1 192.0.2.7 443 x",
            b"KFDC1 192.0.2.7 0",
            b"KFDC1 host 443",
            b"KFDC1  192.0.2.7 443",
            b"\xff",
        ] {
            assert_eq!(
                parse_preamble(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// A header made as the client makes it (the tag and dc field encrypted by the client's own
    /// stream, the first 56 bytes sent as they are) is read back, and both streams agree with the
    /// client's.
    #[test]
    fn an_obfuscated2_header_round_trips() {
        let mut h = [0u8; 64];
        for (i, b) in h.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        h[0] = 0x42;
        h[56..60].copy_from_slice(&0xeeee_eeeeu32.to_le_bytes());
        h[60..62].copy_from_slice(&10002i16.to_le_bytes());
        let mut client_enc = Ctr::new((&h[8..40]).into(), (&h[40..56]).into());
        let mut enc = h;
        client_enc.apply_keystream(&mut enc);
        let mut sent = h;
        sent[56..].copy_from_slice(&enc[56..]);

        let mut header = Header::read(&sent);
        assert_eq!(header.tag, 0xeeee_eeee);
        assert_eq!(header.dc_field, 10002);
        assert_eq!(header.dc_id(), 2);

        // client -> server after the header
        let mut msg = *b"hello server";
        client_enc.apply_keystream(&mut msg);
        header.from_client.apply_keystream(&mut msg);
        assert_eq!(&msg, b"hello server");

        // server -> client
        let mut rev = h;
        rev.reverse();
        let mut client_dec = Ctr::new((&rev[8..40]).into(), (&rev[40..56]).into());
        let mut f = frame(&mut header.to_client, b"abcd");
        client_dec.apply_keystream(&mut f);
        assert_eq!(&f[..4], &4u32.to_le_bytes());
        assert_eq!(&f[4..], b"abcd");
    }
}
