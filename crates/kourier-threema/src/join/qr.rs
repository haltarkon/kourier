//! The join offer and the text of its QR code (TH4, design §2.10).
//!
//! A new device that wants to join a Threema ID's device group makes an [`Offer`]: a fresh 32 byte
//! authentication key (AK) and a fresh rendezvous path on the relay. The QR code carries
//! `DeviceGroupJoinRequestOrOffer` (`url-payloads.proto`), formed as Threema Desktop's `getJoinUri`
//! (`rendezvous.ts`) forms it: join protocol version `V1_0`, the variant `request_to_join` (this device is
//! the initiator and not the nominator), ONE relayed WebSocket path (path id 1, network cost unknown, the
//! relay URL) and no direct TCP server (a listening socket is the dial module's business, and Desktop
//! offers none). The text is `threema://device-group/join#` and the URL-safe base64 of the protobuf, WITH
//! the `=` padding (Desktop: `btoa`, then only `+` -> `-` and `/` -> `_`).
//!
//! The D2D protocol version in the message is `V0_3` and NOT Desktop's `V0_2` (decision C9: what
//! libthreema's own reflections carry). [U: whether the Android app accepts `V0_3` in a join request; the
//! first real link (RT1) shows it.]
//!
//! The AK, the path, the relay URL (it holds the path) and the text are secrets: they are held in
//! `Zeroizing`, and `Offer`'s `Debug` prints nothing of them.

use std::fmt::Write as _;

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use libthreema::{
    common::config::RendezvousServerBaseUrl,
    protobuf::{
        common::Unit,
        d2d::ProtocolVersion,
        d2d_rendezvous::{
            RendezvousInit,
            rendezvous_init::{NetworkCost, RelayedWebSocket, Version as RendezvousVersion},
        },
    },
};
use prost::Message as _;
use rand::CryptoRng;
use zeroize::{Zeroize as _, Zeroizing};

use crate::proto::url::{
    DeviceGroupJoinRequestOrOffer,
    device_group_join_request_or_offer::{Variant, Version as JoinVersion, variant::Type},
};

/// The start of every join QR text (the consumer flavour's custom URI scheme and the path).
const PREFIX: &str = "threema://device-group/join#";

/// The D2D protocol version of the offer: `V0_3` (decision C9, M2), pinned by a test.
const D2D_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V03;

/// The path id of the one relayed WebSocket path (Desktop's `pathId`).
pub(crate) const RELAY_PATH_ID: u32 = 1;

/// What can go wrong making an offer: the relay URL template of the servers' data is no `wss://` base URL.
/// The text names no input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum OfferError {
    /// The rendezvous URL template is not a `wss://` base URL (host, path, trailing slash).
    #[error("rendezvous url template is invalid")]
    Template,
}

/// A join offer: the key, the path and the relay URL of one link attempt.
pub(crate) struct Offer {
    /// The 32 byte authentication key of the rendezvous.
    ak: Zeroizing<[u8; 32]>,
    /// The relay URL, with the path (64 lowercase hex digits for the 32 random bytes) at its end.
    url: Zeroizing<String>,
}

/// Prints nothing of the key, the path or the URL.
impl std::fmt::Debug for Offer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Offer")
    }
}

impl Offer {
    /// A fresh offer: the key from `rng` first (32 bytes), then the rendezvous path (32 bytes, written as 64
    /// lowercase hex digits), and the relay URL out of `rendezvous_url`, the servers' data's template
    /// (`ServerParams::rendezvous_url`) filled by libthreema's own rule for its prefixes (patch P11).
    pub(crate) fn new(rng: &mut impl CryptoRng, rendezvous_url: &str) -> Result<Self, OfferError> {
        let mut ak = Zeroizing::new([0_u8; 32]);
        rng.fill_bytes(&mut *ak);
        let mut path = Zeroizing::new([0_u8; 32]);
        rng.fill_bytes(&mut *path);
        Self::from_parts(rendezvous_url, ak, &path)
    }

    /// The offer for the given key and path (what `new` draws; the tests give fixed ones).
    fn from_parts(
        rendezvous_url: &str,
        ak: Zeroizing<[u8; 32]>,
        path: &[u8; 32],
    ) -> Result<Self, OfferError> {
        let template = RendezvousServerBaseUrl::try_from(rendezvous_url.to_owned())
            .map_err(|_| OfferError::Template)?;
        let mut hex = Zeroizing::new(String::with_capacity(64));
        for byte in path {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
        }
        let url = Zeroizing::new(template.path(path, format_args!("{}", hex.as_str())));
        Ok(Self { ak, url })
    }

    /// The authentication key (for the rendezvous protocol of the join task).
    pub(crate) fn ak(&self) -> &[u8; 32] {
        &self.ak
    }

    /// The relay URL to connect to (it holds the rendezvous path: never log it).
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The text of the QR code.
    pub(crate) fn text(&self) -> Zeroizing<String> {
        let mut message = self.message();
        let bytes = Zeroizing::new(message.encode_to_vec());
        // The message held copies of the key and the URL.
        if let Some(init) = message.rendezvous_init.as_mut() {
            init.ak.zeroize();
            if let Some(relay) = init.relayed_web_socket.as_mut() {
                relay.url.zeroize();
            }
        }
        let mut text = Zeroizing::new(String::with_capacity(
            PREFIX.len() + bytes.len().div_ceil(3) * 4,
        ));
        text.push_str(PREFIX);
        // URL-safe alphabet WITH `=` padding, as Desktop writes it.
        URL_SAFE.encode_string(&*bytes, &mut text);
        text
    }

    /// The payload of the QR code.
    fn message(&self) -> DeviceGroupJoinRequestOrOffer {
        DeviceGroupJoinRequestOrOffer {
            version: JoinVersion::V10 as i32,
            d2d_protocol_version: D2D_PROTOCOL_VERSION as u32,
            variant: Some(Variant {
                r#type: Some(Type::RequestToJoin(Unit {})),
            }),
            rendezvous_init: Some(RendezvousInit {
                version: RendezvousVersion::V10 as i32,
                ak: self.ak.to_vec(),
                relayed_web_socket: Some(RelayedWebSocket {
                    path_id: RELAY_PATH_ID,
                    network_cost: NetworkCost::Unknown as i32,
                    url: self.url.to_string(),
                }),
                direct_tcp_server: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::{CryptoRng, RngCore, SeedableRng as _, rngs::StdRng};

    use super::*;

    /// Test servers' data: a `.invalid` host (RFC 6761) with libthreema's two placeholders.
    const TEMPLATE: &str =
        "wss://rendezvous-{rendezvousPathPrefix4}.example.invalid/{rendezvousPathPrefix8}/";

    /// A random source that counts up: the first `fill_bytes` of 32 gives `00 01 .. 1f`, the next
    /// `20 21 .. 3f`.
    struct Counting(u8);

    impl RngCore for Counting {
        fn next_u32(&mut self) -> u32 {
            let mut bytes = [0_u8; 4];
            self.fill_bytes(&mut bytes);
            u32::from_le_bytes(bytes)
        }

        fn next_u64(&mut self) -> u64 {
            let mut bytes = [0_u8; 8];
            self.fill_bytes(&mut bytes);
            u64::from_le_bytes(bytes)
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for byte in dest {
                *byte = self.0;
                self.0 = self.0.wrapping_add(1);
            }
        }
    }

    impl CryptoRng for Counting {}

    /// A path whose first byte is `first` and whose other bytes are `00 01 02 ..`.
    fn path_starting_with(first: u8) -> [u8; 32] {
        let mut path: [u8; 32] = std::array::from_fn(|i| u8::try_from(i).unwrap());
        path[0] = first;
        path
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The message a text carries, decoded with the URL-safe alphabet (padded).
    fn decode(text: &str) -> DeviceGroupJoinRequestOrOffer {
        let fragment = text
            .strip_prefix(PREFIX)
            .expect("the text starts with the join scheme");
        let bytes = URL_SAFE
            .decode(fragment)
            .expect("the fragment is padded URL-safe base64");
        DeviceGroupJoinRequestOrOffer::decode(bytes.as_slice())
            .expect("the fragment is the protobuf")
    }

    /// The URL of a fixed path below 0x10: prefix4 is ONE digit (`0`), prefix8 TWO (`0a`), then the path as 64
    /// hex digits. Patch P9's rule, reached through P11's public `path`.
    #[test]
    fn the_url_of_a_path_below_0x10_has_two_digit_prefix8() {
        let path = path_starting_with(0x0a);
        let offer = Offer::from_parts(TEMPLATE, Zeroizing::new([0_u8; 32]), &path).unwrap();
        let expected = format!("wss://rendezvous-0.example.invalid/0a/{}", hex(&path));
        assert_eq!(offer.url(), expected, "prefix4 `0`, prefix8 `0a`");
        let tail = offer.url().rsplit('/').next().unwrap();
        assert_eq!(tail.len(), 64, "the path is 64 hex digits");
        assert!(
            tail.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "the path is lower-case hex"
        );
    }

    /// From 0x10 up both prefixes are plain: `1c` -> `1` and `1c`, `ff` -> `f` and `ff`.
    #[test]
    fn the_url_of_a_path_from_0x10_has_the_nibble_and_the_byte() {
        for (first, prefix4, prefix8) in
            [(0x10_u8, "1", "10"), (0x1c, "1", "1c"), (0xff, "f", "ff")]
        {
            let path = path_starting_with(first);
            let offer = Offer::from_parts(TEMPLATE, Zeroizing::new([0_u8; 32]), &path).unwrap();
            let expected = format!(
                "wss://rendezvous-{prefix4}.example.invalid/{prefix8}/{}",
                hex(&path)
            );
            assert_eq!(offer.url(), expected, "first byte {first:#04x}");
        }
    }

    /// A template that is no `wss://` base URL is refused with a fixed sentence.
    #[test]
    fn a_bad_template_is_refused() {
        for template in [
            "",
            "https://x.example.invalid/",
            "wss://x.example.invalid/no-trailing-slash",
        ] {
            let err = Offer::new(&mut Counting(0), template).unwrap_err();
            assert_eq!(err, OfferError::Template, "template {template:?}");
            assert_eq!(err.to_string(), "rendezvous url template is invalid");
        }
    }

    /// `new` draws the key first and the path second, 32 bytes each: from a counting source the key is
    /// `00..1f` and the path `20..3f`.
    #[test]
    fn new_draws_the_key_then_the_path() {
        let offer = Offer::new(&mut Counting(0), TEMPLATE).unwrap();
        let ak: [u8; 32] = std::array::from_fn(|i| u8::try_from(i).unwrap());
        assert_eq!(offer.ak(), &ak, "the key is the first 32 bytes");
        let path: [u8; 32] = std::array::from_fn(|i| u8::try_from(0x20 + i).unwrap());
        let expected = format!("wss://rendezvous-2.example.invalid/20/{}", hex(&path));
        assert_eq!(offer.url(), expected, "the path is the next 32 bytes");
    }

    /// The text decodes back to every field of Desktop's `getJoinUri`, with `V0_3` as the D2D protocol
    /// version (decision C9; Desktop writes `V0_2`).
    #[test]
    fn the_text_decodes_back_to_every_field() {
        let offer = Offer::new(&mut Counting(0), TEMPLATE).unwrap();
        let text = offer.text();
        assert!(text.starts_with("threema://device-group/join#"));
        let message = decode(&text);
        assert_eq!(message.version, 0, "join protocol V1_0");
        assert_eq!(
            message.d2d_protocol_version, 3,
            "D2D protocol V0_3, not Desktop's V0_2"
        );
        assert_eq!(message.d2d_protocol_version, ProtocolVersion::V03 as u32);
        let variant = message.variant.expect("the variant is set");
        assert_eq!(
            variant.r#type,
            Some(Type::RequestToJoin(Unit {})),
            "request to join, no offer"
        );
        let init = message.rendezvous_init.expect("the rendezvous init is set");
        assert_eq!(
            init.version,
            RendezvousVersion::V10 as i32,
            "rendezvous V1_0"
        );
        assert_eq!(init.ak, offer.ak().to_vec(), "the key");
        assert!(init.direct_tcp_server.is_none(), "no direct TCP path");
        let relay = init.relayed_web_socket.expect("one relayed WebSocket path");
        assert_eq!(relay.path_id, 1);
        assert_eq!(relay.network_cost, NetworkCost::Unknown as i32);
        assert_eq!(relay.url, offer.url(), "the relay URL");
    }

    /// The alphabet is the URL-safe one: a key of `0xfb` bytes makes a standard text with `+` and `/`
    /// (`fb fb fb` is `+/v7`), here `-` and `_`.
    #[test]
    fn the_alphabet_is_url_safe() {
        let offer = Offer::from_parts(
            TEMPLATE,
            Zeroizing::new([0xfb_u8; 32]),
            &path_starting_with(0x11),
        )
        .unwrap();
        let text = offer.text();
        let fragment = &text[PREFIX.len()..];
        assert!(fragment.contains("-_v7"), "URL-safe `-_v7` expected");
        assert!(
            !fragment.contains('+') && !fragment.contains('/'),
            "no standard-alphabet character"
        );
        assert!(
            fragment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b)),
            "only the URL-safe alphabet and `=`"
        );
    }

    /// The `=` padding is kept (Desktop: `btoa`, then only `+` and `/` change): the fragment's length is a
    /// multiple of four and it ends in as many `=` as the protobuf's length asks, for each of the three
    /// remainders of that length (the host name's length moves it).
    #[test]
    fn the_padding_is_kept() {
        let mut remainders = std::collections::BTreeSet::new();
        for host in ["a", "aa", "aaa"] {
            let template = format!("wss://{host}.example.invalid/{{rendezvousPathPrefix8}}/");
            let offer = Offer::from_parts(
                &template,
                Zeroizing::new([7_u8; 32]),
                &path_starting_with(0x33),
            )
            .unwrap();
            let text = offer.text();
            let fragment = &text[PREFIX.len()..];
            let length = offer.message().encoded_len();
            remainders.insert(length % 3);
            assert_eq!(
                fragment.len() % 4,
                0,
                "a padded text has a length divisible by four"
            );
            let padding = fragment.bytes().rev().take_while(|b| *b == b'=').count();
            assert_eq!(
                padding,
                (3 - length % 3) % 3,
                "`=` count for a protobuf of {length} bytes"
            );
        }
        assert_eq!(
            remainders.len(),
            3,
            "all three remainders were tried, padding of 0, 1 and 2"
        );
    }

    /// The key is 32 bytes, and two offers from one source differ in the key, the path and the text.
    #[test]
    fn two_offers_differ_and_the_key_has_32_bytes() {
        let mut rng = StdRng::seed_from_u64(0x0074_6872_6565_6d61);
        let first = Offer::new(&mut rng, TEMPLATE).unwrap();
        let second = Offer::new(&mut rng, TEMPLATE).unwrap();
        assert_eq!(first.ak().len(), 32);
        assert_ne!(first.ak(), second.ak(), "the keys differ");
        assert_ne!(first.url(), second.url(), "the paths differ");
        assert_ne!(*first.text(), *second.text(), "the texts differ");
        assert_ne!(first.ak(), &[0_u8; 32], "the key is drawn, not zero");
    }

    /// `Debug` prints nothing of the key, the path or the URL.
    #[test]
    fn debug_prints_nothing_secret() {
        let offer = Offer::new(&mut Counting(0), TEMPLATE).unwrap();
        assert_eq!(format!("{offer:?}"), "Offer");
        assert_eq!(format!("{offer:#?}"), "Offer");
    }
}
