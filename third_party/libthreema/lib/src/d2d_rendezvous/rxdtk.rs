//! Rendezvous Initiator/Responder Device Transport Key (RIDTK/RRDTK) utilities.
use libthreema_dependencies::crypto::{
    blake2b, chacha20, consts,
    digest::{FixedOutput as _, Mac as _},
    generic_array,
};
use libthreema_macros::concat_fixed_bytes;

use super::{EphemeralTransportKey, RendezvousAuthenticationKey, RendezvousPathHash, rxdak, rxdxk};

#[inline]
fn derive_stk(
    ak: &RendezvousAuthenticationKey,
    etk: &EphemeralTransportKey,
) -> generic_array::GenericArray<u8, consts::U32> {
    // Derive STK from AK and ETK
    let key: [u8; 64] = concat_fixed_bytes!(ak.0, etk.0.to_bytes());
    let mut stk = generic_array::GenericArray::default();
    blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&key), b"st", b"3ma-rendezvous")
        .expect("Blake2bMac256 failed")
        .finalize_into(&mut stk);
    stk
}

#[inline]
fn derive_keys(
    ak: &RendezvousAuthenticationKey,
    etk: &EphemeralTransportKey,
) -> (
    chacha20::Key,
    chacha20::Key,
    generic_array::GenericArray<u8, consts::U32>,
) {
    // Derive STK from AK and ETK
    let stk = derive_stk(ak, etk);

    // Derive RIDTK, RRDTK and RPH
    let ridtk = blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&stk), b"ridt", b"3ma-rendezvous")
        .expect("Blake2bMac256 failed")
        .finalize_fixed();
    let rrdtk = blake2b::Blake2bMac256::new_with_salt_and_personal(Some(&stk), b"rrdt", b"3ma-rendezvous")
        .expect("Blake2bMac256 failed")
        .finalize_fixed();
    let rph = derive_rph(&stk);
    (ridtk, rrdtk, rph)
}

/// Derive the Rendezvous Path Hash (RPH) from STK: unkeyed BLAKE2b-256 with salt and persona (this needs the patched
/// blake2, `None` = no key block).
#[inline]
fn derive_rph(stk: &generic_array::GenericArray<u8, consts::U32>) -> generic_array::GenericArray<u8, consts::U32> {
    blake2b::Blake2bMac256::new_with_salt_and_personal(None, b"ph", b"3ma-rendezvous")
        .expect("Blake2bMac256 failed")
        .chain_update(stk)
        .finalize_fixed()
}

/// RID's encrypt/decrypt context used after authentication.
pub(super) struct ForRid {
    pub(super) ridtk: rxdxk::Encrypt,
    pub(super) rrdtk: rxdxk::Decrypt,
}

/// RRD's encrypt/decrypt context used after authentication.
pub(super) struct ForRrd {
    pub(super) ridtk: rxdxk::Decrypt,
    pub(super) rrdtk: rxdxk::Encrypt,
}

impl ForRid {
    #[expect(clippy::needless_pass_by_value, reason = "Prevent key re-use")]
    pub(super) fn new(
        ak: &RendezvousAuthenticationKey,
        rxdak: rxdak::ForRid,
        etk: EphemeralTransportKey,
    ) -> (Self, RendezvousPathHash) {
        let (ridtk, rrdtk, rph) = derive_keys(ak, &etk);
        let rxdtk = Self {
            ridtk: rxdxk::Encrypt::new_from(rxdak.ridak, ridtk),
            rrdtk: rxdxk::Decrypt::new_from(rxdak.rrdak, rrdtk),
        };
        (rxdtk, RendezvousPathHash(rph.into()))
    }
}

impl ForRrd {
    #[expect(clippy::needless_pass_by_value, reason = "Prevent key re-use")]
    pub(super) fn new(
        ak: &RendezvousAuthenticationKey,
        rxdak: rxdak::ForRrd,
        etk: EphemeralTransportKey,
    ) -> (Self, RendezvousPathHash) {
        let (ridtk, rrdtk, rph) = derive_keys(ak, &etk);
        let rxdtk = Self {
            ridtk: rxdxk::Decrypt::new_from(rxdak.ridak, ridtk),
            rrdtk: rxdxk::Encrypt::new_from(rxdak.rrdak, rrdtk),
        };
        (rxdtk, RendezvousPathHash(rph.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answer for the RPH (the value the emoji are made from), computed independently of this crate and of its
    /// patched blake2, with Python's standard library:
    ///
    /// ```text
    /// python3 -I -c 'import hashlib; print(hashlib.blake2b(bytes(range(32)), digest_size=32, salt=b"ph",
    ///     person=b"3ma-rendezvous").hexdigest())'
    /// 4e84f0da1921ba5758fb6ff9ee6d84503e01c4d22828e488600c4e21484746cc
    /// ```
    #[test]
    fn the_path_hash_of_a_fixed_stk_matches_python() {
        let stk: [u8; 32] = core::array::from_fn(|index| u8::try_from(index).expect("index < 32"));
        let rph = derive_rph(&generic_array::GenericArray::from(stk));
        let expected: [u8; 32] = [
            0x4e, 0x84, 0xf0, 0xda, 0x19, 0x21, 0xba, 0x57, 0x58, 0xfb, 0x6f, 0xf9, 0xee, 0x6d, 0x84, 0x50, 0x3e, 0x01,
            0xc4, 0xd2, 0x28, 0x28, 0xe4, 0x88, 0x60, 0x0c, 0x4e, 0x21, 0x48, 0x47, 0x46, 0xcc,
        ];
        assert_eq!(
            rph.as_slice(),
            expected.as_slice(),
            "the RPH must be unkeyed BLAKE2b-256 of STK, salt `ph`, persona `3ma-rendezvous`"
        );
    }
}
