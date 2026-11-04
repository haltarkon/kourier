//! The fake's crypto (`support/mock/crypto.rs`): proof 1 of lane plan §5.2 (Desktop's three
//! device-group-key vectors; the path key's public half equals what the real `D2mProtocol::new`
//! puts into its URL) and the helpers the servers build on.

use libthreema::common::config::MediatorServerBaseUrl;
use libthreema::common::keys::DeviceGroupKey;
use libthreema::common::{ChatServerGroup, ClientInfo, D2xDeviceId};
use libthreema::d2m::{D2mContext, D2mProtocol};
use libthreema::protobuf::d2m;
use prost::Message as _;

use super::{unhex, unhex32};
use crate::support::mock::crypto::{
    CspNonces, DeviceGroupKeys, KeyPair, csp_nonce, derive, hsalsa_shared, open, open_nonce_ahead,
    seal, seal_nonce_ahead, server_vouch, vouch_from_secrets, x25519_public,
};
use crate::support::mock::desktop_vectors::DGK_VECTORS;
use crate::support::mock::world;

/// Desktop's THREE vectors (critic L1): every key equals the hex Desktop's deriver gives.
#[test]
fn desktops_three_vectors_derive_as_desktop_does() {
    assert_eq!(DGK_VECTORS.len(), 3, "Desktop's file holds three vectors");
    for (n, v) in DGK_VECTORS.iter().enumerate() {
        let dgk = unhex32(v.dgk);
        let keys = DeviceGroupKeys::derive(&dgk);
        let pairs = [
            ("DGPK", keys.path_secret, v.dgpk),
            ("DGRK", keys.reflect, v.dgrk),
            ("DGDIK", keys.device_info, v.dgdik),
            ("DGSDDK", keys.shared_device_data, v.dgsddk),
            ("DGTSK", keys.transaction_scope, v.dgtsk),
        ];
        for (name, got, want) in pairs {
            assert!(
                got == unhex32(want),
                "vector {n}: the fake's {name} differs from Desktop's"
            );
        }
        // The single-key form agrees with the struct.
        assert!(
            derive(&dgk, b"di") == keys.device_info,
            "vector {n}: derive(di)"
        );
    }
}

/// The mediator's URL template of the tests: an `.invalid` host (RFC 6761) with libthreema's two
/// placeholders, so the URL also shows the prefix of the path key.
const MEDIATOR_TEMPLATE: &str =
    "wss://mediator-{deviceGroupIdPrefix4}.invalid/{deviceGroupIdPrefix8}/";

fn client_url(dgk: [u8; 32], group: &str) -> String {
    let context = D2mContext {
        mediator_server_url: MediatorServerBaseUrl::try_from(MEDIATOR_TEMPLATE.to_owned())
            .expect("the test's mediator URL"),
        device_group_key: DeviceGroupKey::from(dgk),
        csp_server_group: ChatServerGroup::try_from(group).expect("a server group"),
        device_id: D2xDeviceId(0x8000_0000_0000_0001),
        device_slots_exhausted_policy: d2m::client_hello::DeviceSlotsExhaustedPolicy::Reject,
        device_slot_expiration_policy: d2m::DeviceSlotExpirationPolicy::Persistent,
        expected_device_slot_state: d2m::DeviceSlotState::New,
        client_info: ClientInfo::Libthreema,
        device_label: String::new(),
    };
    D2mProtocol::new(context).1
}

/// DGPK.public of the fake equals the `device_group_id` the real client puts into the URL's
/// `ClientUrlInfo`, for Desktop's three DGKs and one of the world's; the host prefix is the first
/// nibble of that key (one hex digit) and the path prefix its first byte as TWO hex digits, also
/// below 0x10 (libthreema's patch P9; Desktop's third vector starts below 0x10).
#[test]
fn the_path_keys_public_half_is_what_the_real_clients_url_carries() {
    let w = world(11);
    let mut dgks: Vec<[u8; 32]> = DGK_VECTORS.iter().map(|v| unhex32(v.dgk)).collect();
    dgks.push(w.new_device_group_key());
    for (n, dgk) in dgks.into_iter().enumerate() {
        let keys = DeviceGroupKeys::derive(&dgk);
        let url = client_url(dgk, "ds-7");
        let first = keys.path_public[0];
        let head = format!("wss://mediator-{:x}.invalid/{first:02x}/", first >> 4);
        assert!(
            url.starts_with(&head),
            "case {n}: the URL's prefixes are not the path key's"
        );
        let info_hex = &url[head.len()..];
        let info = d2m::ClientUrlInfo::decode(unhex(info_hex).as_slice())
            .expect("the URL's last segment is a ClientUrlInfo");
        assert!(
            info.device_group_id == keys.path_public,
            "case {n}: the client's device group id is not the fake's DGPK.public"
        );
        assert!(
            info.server_group == "ds-7",
            "case {n}: the server group differs"
        );
    }
}

#[test]
fn x25519_hsalsa20_agrees_both_ways_and_refuses_a_low_order_key() {
    let w = world(12);
    let (a, b) = (w.new_key_pair(), w.new_key_pair());
    let ab = hsalsa_shared(&a.secret, &b.public).expect("contributory");
    let ba = hsalsa_shared(&b.secret, &a.public).expect("contributory");
    assert!(ab == ba, "X25519HSalsa20 differs between the two sides");
    assert!(
        ab != hsalsa_shared(&a.secret, &w.new_key_pair().public).expect("contributory"),
        "another peer gives the same secret"
    );
    // The all-zero point is of low order: no secret, as libthreema refuses it.
    assert!(
        hsalsa_shared(&a.secret, &[0u8; 32]).is_none(),
        "a low-order key was accepted"
    );
    assert!(
        x25519_public(&a.secret) == a.public,
        "the key pair's public half"
    );
}

#[test]
fn nonce_ahead_boxes_open_only_with_their_key_and_whole() {
    let w = world(13);
    let key: [u8; 32] = w.random();
    let nonce: [u8; 24] = w.random();
    let boxed = seal_nonce_ahead(&key, &nonce, b"payload");
    assert_eq!(boxed.len(), 24 + 16 + 7, "nonce || tag || ciphertext");
    assert!(boxed[..24] == nonce, "the nonce comes first");
    assert!(
        open_nonce_ahead(&key, &boxed).as_deref() == Some(&b"payload"[..]),
        "the box does not open"
    );
    let other: [u8; 32] = w.random();
    assert!(
        open_nonce_ahead(&other, &boxed).is_none(),
        "another key opened the box"
    );
    assert!(
        open_nonce_ahead(&key, &boxed[1..]).is_none(),
        "a suffix opened"
    );
    assert!(
        open_nonce_ahead(&key, &boxed[..20]).is_none(),
        "a short box opened"
    );
    let mut flipped = boxed.clone();
    *flipped.last_mut().expect("bytes") ^= 1;
    assert!(
        open_nonce_ahead(&key, &flipped).is_none(),
        "a changed box opened"
    );
}

/// `cookie || u64-le(n)`, n from 1, each number once; a box of the wrong number does not open and
/// does not move the counter (the fake's strict counter).
#[test]
fn csp_nonces_count_from_one_and_never_skip() {
    let w = world(14);
    let key: [u8; 32] = w.random();
    let cookie: [u8; 16] = w.random();
    let nonce = csp_nonce(&cookie, 0x0102_0304_0506_0708);
    assert!(nonce[..16] == cookie, "the cookie comes first");
    assert!(
        nonce[16..] == [8, 7, 6, 5, 4, 3, 2, 1],
        "the sequence number is u64 little-endian"
    );

    let mut sender = CspNonces::new(cookie);
    assert_eq!(sender.next_number(), 1, "the counter starts at 1");
    let first = sender.seal(&key, b"one").expect("seal 1");
    let second = sender.seal(&key, b"two").expect("seal 2");
    assert_eq!(sender.next_number(), 3, "two boxes used two numbers");
    assert!(
        open(&key, &csp_nonce(&cookie, 1), &first).is_some(),
        "the first box is sealed with number 1"
    );

    let mut receiver = CspNonces::new(cookie);
    assert!(
        receiver.open(&key, &second).is_none(),
        "a skipped number opened"
    );
    assert_eq!(receiver.next_number(), 1, "a refused box moved the counter");
    assert!(
        receiver.open(&key, &first).as_deref() == Some(&b"one"[..]),
        "box 1"
    );
    assert!(
        receiver.open(&key, &first).is_none(),
        "a repeated number opened"
    );
    assert!(
        receiver.open(&key, &second).as_deref() == Some(&b"two"[..]),
        "box 2"
    );
    assert!(
        seal(&key, &nonce, b"x") != seal(&key, &csp_nonce(&cookie, 1), b"x"),
        "nonce"
    );
}

/// The server's vouch equals the one the client computes from `CK.secret` and the two server
/// publics (the cross-check with libthreema's own client is M4's login test).
#[test]
fn the_server_and_the_client_compute_the_same_vouch() {
    let w = world(15);
    let ck = w.new_key_pair();
    let sk = w.server_key().clone();
    let tsk = w.new_key_pair();
    let tck = w.new_key_pair();
    let sck: [u8; 16] = w.random();
    let server =
        server_vouch(&sk.secret, &tsk.secret, &ck.public, &sck, &tck.public).expect("contributory");
    let client = vouch_from_secrets(
        &hsalsa_shared(&ck.secret, &sk.public).expect("ss1"),
        &hsalsa_shared(&ck.secret, &tsk.public).expect("ss2"),
        &sck,
        &tck.public,
    );
    assert!(server == client, "the two sides' vouches differ");
    let other_cookie: [u8; 16] = w.random();
    let other = server_vouch(
        &sk.secret,
        &tsk.secret,
        &ck.public,
        &other_cookie,
        &tck.public,
    )
    .expect("contributory");
    assert!(
        server != other,
        "the vouch does not depend on the server's cookie"
    );
    let stranger = KeyPair::from_secret(w.random());
    let theirs = server_vouch(&sk.secret, &tsk.secret, &stranger.public, &sck, &tck.public)
        .expect("contributory");
    assert!(
        server != theirs,
        "the vouch does not depend on the client's key"
    );
}

/// TH4 J2: the joiner's path hash is UNKEYED BLAKE2b-256 of STK with salt `ph` and personal
/// `3ma-rendezvous`: the known answer that patch P3's test pins too, computed by Python's hashlib
/// (`hashlib.blake2b(bytes(range(32)), digest_size=32, salt=b"ph", person=b"3ma-rendezvous")`).
#[test]
fn the_joiners_path_hash_is_unkeyed_blake2b_as_python_computes_it() {
    use crate::support::mock::joiner::path_hash;
    let stk: [u8; 32] = core::array::from_fn(|i| u8::try_from(i).expect("i < 32"));
    let expected = unhex32("4e84f0da1921ba5758fb6ff9ee6d84503e01c4d22828e488600c4e21484746cc");
    assert!(path_hash(&stk) == expected, "the path hash is not Python's");
}

/// TH4 J2: the joiner's own URL-safe base64 keeps the padding and reads what it wrote, for every
/// remainder, and refuses the standard alphabet and a missing pad.
#[test]
fn the_joiners_base64_is_url_safe_with_padding() {
    use crate::support::mock::joiner::{url_safe_base64, url_safe_base64_decode};
    assert_eq!(
        url_safe_base64(&[0xfb, 0xff]),
        "-_8=",
        "the url-safe alphabet"
    );
    assert_eq!(url_safe_base64(b"ab"), "YWI=");
    assert_eq!(url_safe_base64(b"a"), "YQ==");
    assert_eq!(url_safe_base64(b"abc"), "YWJj");
    for n in 0..40usize {
        let bytes: Vec<u8> = (0..n)
            .map(|i| u8::try_from(i * 37 % 256).expect("a byte"))
            .collect();
        let text = url_safe_base64(&bytes);
        assert_eq!(text.len() % 4, 0, "unpadded");
        if n > 0 {
            assert_eq!(url_safe_base64_decode(&text), Some(bytes), "no round trip");
        }
    }
    assert_eq!(
        url_safe_base64_decode("+/8="),
        None,
        "the standard alphabet read"
    );
    assert_eq!(url_safe_base64_decode("YWI"), None, "a missing pad read");
    assert_eq!(
        url_safe_base64_decode("YQ==YQ=="),
        None,
        "a pad in the middle read"
    );
}

/// libthreema's patch P5 against the fake's OWN crypto (the two-implementation check of TH5, brief
/// §2.2): the fake seals a reflected envelope with its own BLAKE2b derivation of DGRK (salt `r`) and
/// its own secretbox, and libthreema's public `DeviceGroupKey::open_reflected` opens it to the same
/// nonce and bytes; a box of another derived key (DGTSK, salt `ts`) or of another group is refused.
#[test]
fn the_fakes_seal_opens_with_p5() {
    let dgk = [0x5c; 32];
    let keys = DeviceGroupKeys::derive(&dgk);
    let nonce = [0x07; 24];
    let plaintext = b"the padded bytes of a d2d.Envelope".to_vec();
    let sealed = seal_nonce_ahead(&keys.reflect, &nonce, &plaintext);
    let (opened_nonce, opened) = DeviceGroupKey::from(dgk)
        .open_reflected(&sealed)
        .expect("libthreema opens the fake's reflected envelope");
    assert!(opened_nonce.0 == nonce, "the nonce ahead is the fake's");
    assert!(opened == plaintext, "the bytes are the fake's");

    let scope_box = seal_nonce_ahead(&keys.transaction_scope, &nonce, &plaintext);
    assert!(
        DeviceGroupKey::from(dgk)
            .open_reflected(&scope_box)
            .is_err(),
        "a DGTSK box must not open as a reflected envelope"
    );
    assert!(
        DeviceGroupKey::from([0x5d; 32])
            .open_reflected(&sealed)
            .is_err(),
        "another group's key must not open it"
    );
}

/// The other direction: what libthreema's public `seal_reflected` seals, the fake opens with its own
/// DGRK; and the same both ways for the transaction scope (DGTSK) and the device info (DGDIK).
#[test]
fn p5s_seals_open_with_the_fake_and_the_fakes_scope_and_device_info_open_with_p5() {
    let dgk = [0x6e; 32];
    let keys = DeviceGroupKeys::derive(&dgk);
    let key = DeviceGroupKey::from(dgk);
    let plaintext = b"an envelope, a scope or a device info".to_vec();

    let (nonce, sealed) = key.seal_reflected(plaintext.clone()).expect("seal");
    assert!(sealed[..24] == nonce.0, "the nonce is ahead");
    assert!(
        open_nonce_ahead(&keys.reflect, &sealed).as_deref() == Some(plaintext.as_slice()),
        "the fake opens libthreema's reflected envelope with its own DGRK"
    );

    let scope = key.seal_transaction_scope(plaintext.clone()).expect("seal");
    assert!(
        open_nonce_ahead(&keys.transaction_scope, &scope).as_deref() == Some(plaintext.as_slice()),
        "the fake opens libthreema's scope with its own DGTSK"
    );
    let fakes_scope = seal_nonce_ahead(&keys.transaction_scope, &[0x01; 24], &plaintext);
    assert!(
        key.open_transaction_scope(&fakes_scope).ok() == Some(plaintext.clone()),
        "libthreema opens the fake's scope"
    );
    let fakes_info = seal_nonce_ahead(&keys.device_info, &[0x02; 24], &plaintext);
    assert!(
        key.open_device_info(&fakes_info).ok() == Some(plaintext.clone()),
        "libthreema opens the fake's device info"
    );
    assert!(
        key.open_device_info(&fakes_scope).is_err(),
        "a DGTSK box must not open as a device info"
    );
}

/// `Nonce::hash_with` (P5) from outside libthreema: HMAC-SHA256 keyed by the identity's 8 bytes over
/// the 24 nonce bytes, the vector libthreema's own `common::nonce_hash_tests` pins (Python's `hmac`,
/// the command is in that test's doc).
#[test]
fn p5s_hash_with_gives_the_nonce_stores_vector() {
    use libthreema::common::{Nonce, ThreemaId};
    let identity = ThreemaId::try_from("ECHOECHO").expect("a valid ID");
    let nonce = Nonce(std::array::from_fn(|i| u8::try_from(i).expect("a byte")));
    assert!(
        nonce.hash_with(identity).0
            == unhex32("b6e30b6b79d591a6d1e0df00010dd9abc43591c8d701fc125b51f8648f0c98f0"),
        "hash_with is not HMAC-SHA256(key = the ID, message = the nonce)"
    );
}
