//! A look below SQLite (G1d, copied from `kourier-signal/tests/history_transfer_attack.rs`):
//! every page of a core.db file and every frame of its WAL decrypted with the file's key,
//! whether or not its transaction committed and whatever generation of the WAL it belongs to.

use aes::Aes256Dec;
use aes::cipher::{BlockCipherDecrypt as _, KeyInit as _};

/// SQLCipher 4's page layout, as `kourier_db::open` uses it (a raw key, the defaults): pages
/// of 4096 bytes whose last 80 are the reserve, which starts with the page's IV; the first
/// 16 bytes of page 1 are the salt and are not encrypted.
const PAGE: usize = 4096;
const RESERVE: usize = 80;
const SALT: usize = 16;
const WAL_HEADER: usize = 32;
const FRAME_HEADER: usize = 24;

/// Decrypts one page image (`number` is 1 for the page that starts with the salt): the
/// plaintext of everything in front of the reserve.
fn decrypt_page(dbkey: &[u8; 32], page: &[u8], number: u32) -> Vec<u8> {
    let cipher = Aes256Dec::new(dbkey.into());
    let body_end = PAGE - RESERVE;
    let start = if number == 1 { SALT } else { 0 };
    let mut previous: [u8; 16] = page[body_end..body_end + 16].try_into().expect("the IV");
    let mut plain = Vec::with_capacity(body_end - start);
    for block in page[start..body_end].as_chunks::<16>().0 {
        let mut out = *block;
        cipher.decrypt_block((&mut out).into());
        plain.extend(out.iter().zip(previous).map(|(a, b)| a ^ b));
        previous = *block;
    }
    plain
}

/// What a look below SQLite found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RawLook {
    /// Pages of the file and frames of the WAL that were decrypted.
    pub pages: usize,
    /// How many of them hold each needle, in the order given.
    pub with: Vec<usize>,
    /// How many hold `proof` (a text of the schema): the decryption gives plaintext.
    pub with_proof: usize,
}

/// Decrypts every page of `db` and every frame of `wal` and counts those that hold each of
/// `needles`, and `proof`.
pub fn raw_look(
    db: &[u8],
    wal: &[u8],
    dbkey: &[u8; 32],
    needles: &[&[u8]],
    proof: &[u8],
) -> RawLook {
    let holds = |hay: &[u8], needle: &[u8]| memchr::memmem::find(hay, needle).is_some();
    let mut look = RawLook {
        with: vec![0; needles.len()],
        ..RawLook::default()
    };
    let mut note = |plain: Vec<u8>| {
        look.pages += 1;
        for (n, needle) in needles.iter().enumerate() {
            look.with[n] += usize::from(holds(&plain, needle));
        }
        look.with_proof += usize::from(holds(&plain, proof));
    };
    for (i, page) in db.as_chunks::<PAGE>().0.iter().enumerate() {
        note(decrypt_page(
            dbkey,
            page,
            u32::try_from(i + 1).expect("page"),
        ));
    }
    let mut at = WAL_HEADER;
    while at + FRAME_HEADER + PAGE <= wal.len() {
        let number = u32::from_be_bytes(wal[at..at + 4].try_into().expect("four bytes"));
        note(decrypt_page(
            dbkey,
            &wal[at + FRAME_HEADER..at + FRAME_HEADER + PAGE],
            number,
        ));
        at += FRAME_HEADER + PAGE;
    }
    look
}

/// [`raw_look`] of the file at `db` and its WAL as they are now (what a kill would leave).
pub fn look_at(db: &std::path::Path, dbkey: &[u8; 32], needles: &[&[u8]], proof: &[u8]) -> RawLook {
    let file = std::fs::read(db).expect("core.db");
    let wal = std::fs::read(db.with_extension("db-wal")).unwrap_or_default();
    raw_look(&file, &wal, dbkey, needles, proof)
}
