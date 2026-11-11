//! PROFILE PICTURES (TH8): the pictures of the account's contacts and groups, as the phone syncs them.
//!
//! The sync says which blob is a contact's or a group's picture: the one the user chose for a contact, else the one the
//! contact chose; a join brings the blobs along (`held_blob`), a later sync leaves them on the mirror. One task per
//! actor walks over what differs from what the core was told ([`due`]), one picture at a time and only while `Online`:
//! when the account comes online and after every contact or group sync, so a picture changed on the phone arrives without
//! being asked for. Only a round that could not reach a picture looks again by the clock ([`LOOK_AGAIN`]).
//!
//! The picture is written as a sealed file BEFORE the zone that records it, and the file it replaces goes after the
//! commit (Signal's order). What is not a JPEG of a sane size is not kept, and the blob is not asked for again until the
//! sync names another.
//!
//! Counts and kinds only in the log: no chat, no id, no key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock};
use kourier_blob::{BlobId, BlobStore, FileKey};
use kourier_db::rusqlite::{Connection, OptionalExtension, params};
use kourier_db::{Tx, ZoneDb};
use kourier_model::{BlobKey, BlobRef, ConnState, ContactPatch, ConvKey, ConversationPatch, Patch};
use libthreema::protobuf::common::{self, DeltaImage};
use libthreema::protobuf::d2d_sync;
use tokio::sync::{Notify, watch};

use crate::blob::{FILE_NONCE, Failure, ID_LEN, KEY_LEN, Mirror, NONCE_LEN, Scope, open};
use crate::error::{Result, StoreError, sql};
use crate::join::events::{Snapshot, group_key, user};
use crate::{outbox, store};

/// How long a round that could not reach a picture rests before it looks again.
pub(crate) const LOOK_AGAIN: Duration = Duration::from_secs(2 * 60);
/// The least time between two pictures.
pub(crate) const GAP: Duration = Duration::from_secs(1);
/// The largest picture kept.
pub(crate) const MAX_PICTURE: usize = 2 * 1024 * 1024;

/// What the walk needs of the actor.
pub(crate) struct Parts {
    pub(crate) mirror: Mirror,
    pub(crate) db: Rc<ZoneDb>,
    pub(crate) conn: watch::Receiver<ConnState>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) blob_dir: PathBuf,
    pub(crate) wake: Rc<Notify>,
    /// Told after a contact or group sync was stored: a picture may differ now.
    pub(crate) changed: Rc<Notify>,
}

/// Lives as long as the actor: a walk for every time the account is `Online`.
pub(crate) async fn run(parts: Parts) {
    let mut conn = parts.conn.clone();
    loop {
        if conn.wait_for(is_online).await.is_err() {
            return;
        }
        let mut leaving = parts.conn.clone();
        tokio::select! {
            changed = leaving.wait_for(|state| !is_online(state)) => {
                if changed.is_err() {
                    return;
                }
            }
            () = walk(&parts) => return,
        }
    }
}

fn is_online(state: &ConnState) -> bool {
    matches!(state, ConnState::Online { .. })
}

/// The rounds of one connection. Ends only when the store is gone.
async fn walk(parts: &Parts) {
    loop {
        let round = match parts.db.read(due) {
            Ok(round) => round,
            Err(_) => {
                tracing::warn!("the profile pictures could not be read");
                return;
            }
        };
        let mut unreached = false;
        for (n, item) in round.into_iter().enumerate() {
            if n > 0 {
                parts.clock.sleep(GAP).await;
            }
            match settle(parts, item).await {
                Settled::Done => {}
                Settled::Later => unreached = true,
                Settled::StoreGone => return,
            }
        }
        if unreached {
            tokio::select! {
                () = parts.clock.sleep(LOOK_AGAIN) => {}
                () = parts.changed.notified() => {}
            }
        } else {
            parts.changed.notified().await;
        }
    }
}

/// A picture's blob as the sync names it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) id: [u8; ID_LEN],
    /// `None`: only a blob held from the join can be read.
    pub(crate) key: Option<[u8; KEY_LEN]>,
    pub(crate) nonce: [u8; NONCE_LEN],
}

/// What differs from what the core was told.
pub(crate) struct Due {
    pub(crate) conv: ConvKey,
    /// `None`: the picture is gone.
    pub(crate) named: Option<Named>,
}

/// Every chat whose picture is not the one the sync names.
fn due(conn: &Connection) -> Result<Vec<Due>> {
    let snapshot = store::load_snapshot(conn)?;
    let mut known = HashMap::new();
    let mut statement = conn
        .prepare_cached("SELECT conv, blob_id FROM picture")
        .map_err(sql("read the pictures failed"))?;
    let mut rows = statement
        .query([])
        .map_err(sql("read the pictures failed"))?;
    while let Some(row) = rows.next().map_err(sql("read the pictures failed"))? {
        let conv: String = row.get(0).map_err(sql("read the pictures failed"))?;
        let id: Option<Vec<u8>> = row.get(1).map_err(sql("read the pictures failed"))?;
        known.insert(conv, id);
    }
    Ok(differing(&snapshot, &known))
}

fn differing(snapshot: &Snapshot, known: &HashMap<String, Option<Vec<u8>>>) -> Vec<Due> {
    let contacts = snapshot.contacts.iter().filter_map(|contact| {
        let conv = ConvKey::Direct(user(&contact.identity).ok()?);
        Some((conv, picture_of_contact(contact)))
    });
    let groups = snapshot.groups.iter().filter_map(|group| {
        let conv = group_key(&group.creator, group.group_id).ok()?;
        Some((conv, named(group.sync.profile_picture.as_ref())))
    });
    contacts
        .chain(groups)
        .filter_map(|(conv, named)| {
            let was = known.get(&conv.encode()).and_then(Option::as_deref);
            let now = named.as_ref().map(|n| &n.id[..]);
            (was != now).then_some(Due { conv, named })
        })
        .collect()
}

/// The picture a contact shows: the one the user chose for them, else the one they chose.
fn picture_of_contact(contact: &d2d_sync::Contact) -> Option<Named> {
    named(contact.user_defined_profile_picture.as_ref())
        .or_else(|| named(contact.contact_defined_profile_picture.as_ref()))
}

fn named(image: Option<&DeltaImage>) -> Option<Named> {
    let common::delta_image::Image::Updated(image) = image?.image.as_ref()? else {
        return None;
    };
    let blob = image.blob.as_ref()?;
    Some(Named {
        id: blob.id.as_slice().try_into().ok()?,
        key: blob.key.as_slice().try_into().ok(),
        nonce: blob.nonce.as_slice().try_into().unwrap_or(FILE_NONCE),
    })
}

/// What one look came to.
enum Outcome {
    /// A picture, its file written.
    Picture { id: [u8; ID_LEN], blob: BlobRef },
    /// The chat has none any more.
    Removed,
    /// The named blob will not give a picture: it is not asked for again.
    Unusable { id: [u8; ID_LEN] },
}

/// What became of one look.
enum Settled {
    Done,
    /// The mirror could not be reached: the next round asks again.
    Later,
    StoreGone,
}

async fn settle(parts: &Parts, item: Due) -> Settled {
    let Due { conv, named } = item;
    let outcome = match named {
        None => Outcome::Removed,
        Some(named) => match fetch(parts, &named).await {
            Some(outcome) => outcome,
            None => return Settled::Later,
        },
    };
    let written = match &outcome {
        Outcome::Picture { blob, .. } => Some(blob.path.clone()),
        _ => None,
    };
    let now = i64::try_from(parts.clock.now_ms()).unwrap_or(i64::MAX);
    let wake = parts.wake.clone();
    match parts.db.zone(|tx| record(tx, &conv, outcome, now, &wake)) {
        Ok(replaced) => {
            if let Some(old) = replaced {
                let _ = std::fs::remove_file(parts.blob_dir.join(old));
            }
            Settled::Done
        }
        Err(_) => {
            tracing::warn!("a profile picture could not be recorded");
            if let Some(path) = written {
                let _ = std::fs::remove_file(parts.blob_dir.join(path));
            }
            Settled::StoreGone
        }
    }
}

/// The picture's bytes, sealed; `None` when it may come later.
async fn fetch(parts: &Parts, named: &Named) -> Option<Outcome> {
    let id = named.id;
    let held = parts
        .db
        .read(|conn| held(conn, &id))
        .map_err(|_| tracing::warn!("a held picture could not be read"))
        .ok()?;
    let from_the_mirror = held.is_none();
    let raw = match held {
        Some(bytes) => bytes,
        None => match parts.mirror.fetch(&id, Scope::Local, MAX_PICTURE).await {
            Ok(bytes) => bytes,
            Err(Failure::Gone) => return Some(Outcome::Unusable { id }),
            Err(Failure::Unreachable) => return None,
        },
    };
    let Some(picture) = plain(&raw, named) else {
        tracing::debug!(len = raw.len(), "a picture that is no JPEG was not kept");
        return Some(Outcome::Unusable { id });
    };
    let dir = parts.blob_dir.clone();
    let written = tokio::task::spawn_blocking(move || write_picture(&dir, &picture)).await;
    let Ok(Ok(blob)) = written else {
        tracing::warn!("a picture could not be written");
        return None;
    };
    if from_the_mirror {
        parts.mirror.mark_done(&id).await;
    }
    Some(Outcome::Picture { id, blob })
}

/// Forgets every picture and removes its file: the next walk fetches and states them again (a `Rebuild`).
pub(crate) fn forget_all(db: &ZoneDb, blob_dir: &Path) -> Result<()> {
    const OP: &str = "forget the pictures failed";
    let paths = db.zone(|tx| {
        let paths = {
            let mut statement = tx
                .prepare("SELECT path FROM picture WHERE path IS NOT NULL")
                .map_err(sql(OP))?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(sql(OP))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql(OP))?
        };
        tx.execute("DELETE FROM picture", []).map_err(sql(OP))?;
        Ok::<_, StoreError>(paths)
    })?;
    for path in paths {
        let _ = std::fs::remove_file(blob_dir.join(path));
    }
    Ok(())
}

/// The JPEG in `raw`: a join's blob is the picture itself (Desktop reads it as such), a mirror's is a box. `None` when it
/// is neither.
fn plain(raw: &[u8], named: &Named) -> Option<Vec<u8>> {
    if is_picture(raw) {
        return Some(raw.to_vec());
    }
    let opened = open(named.key.as_ref()?, &named.nonce, raw)?;
    is_picture(&opened).then_some(opened)
}

/// Whether `bytes` can be a picture: a JPEG of a sane size.
fn is_picture(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_PICTURE && bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

fn held(conn: &Connection, id: &[u8; ID_LEN]) -> Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT data FROM held_blob WHERE id = ?1",
        params![&id[..]],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql("read a held picture failed"))
}

/// Seals `image` into a new blob below `blob_dir` (atomic, 0600).
fn write_picture(blob_dir: &Path, image: &[u8]) -> std::io::Result<BlobRef> {
    let store = BlobStore::open(blob_dir)?;
    let id = BlobId::new_random();
    let key = FileKey::generate();
    let mut writer = store.create(&id, &key).map_err(std::io::Error::other)?;
    std::io::Write::write_all(&mut writer, image)?;
    writer.finish().map_err(std::io::Error::other)?;
    Ok(BlobRef {
        path: id.relative_path(),
        key: BlobKey::new(*key.as_bytes()),
    })
}

/// The zone of one look: the row, the held blob it used, and the statement to the core. Answers the file it replaced.
fn record(
    tx: &mut Tx<'_>,
    conv: &ConvKey,
    outcome: Outcome,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<Option<String>> {
    const OP: &str = "record a picture failed";
    let key = conv.encode();
    let old: Option<String> = tx
        .query_row(
            "SELECT path FROM picture WHERE conv = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql(OP))?
        .flatten();
    let (id, path, avatar): (Option<[u8; ID_LEN]>, Option<String>, Patch<BlobRef>) = match outcome {
        Outcome::Picture { id, blob } => (
            Some(id),
            Some(blob.path.to_string_lossy().into_owned()),
            Patch::Set(blob),
        ),
        Outcome::Removed if old.is_some() => (None, None, Patch::Clear),
        Outcome::Unusable { id } if old.is_some() => (Some(id), None, Patch::Clear),
        Outcome::Removed => (None, None, Patch::Unchanged),
        Outcome::Unusable { id } => (Some(id), None, Patch::Unchanged),
    };
    tx.execute(
        "INSERT INTO picture(conv, blob_id, path, at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (conv) DO UPDATE SET blob_id = excluded.blob_id, path = excluded.path, at = excluded.at",
        params![key, id.as_ref().map(|id| &id[..]), path, now],
    )
    .map_err(sql(OP))?;
    if let Some(id) = id {
        tx.execute("DELETE FROM held_blob WHERE id = ?1", params![&id[..]])
            .map_err(sql(OP))?;
    }
    if let Some(event) = statement_of(conv, avatar) {
        outbox::append_events(tx, std::slice::from_ref(&event), now, wake)?;
    }
    Ok(old)
}

/// What the core is told of a picture; `None` when there is nothing to tell.
fn statement_of(conv: &ConvKey, avatar: Patch<BlobRef>) -> Option<BackendEvent> {
    if matches!(avatar, Patch::Unchanged) {
        return None;
    }
    match conv {
        ConvKey::Direct(person) => {
            let mut patch = ContactPatch::new(person.clone());
            patch.avatar = avatar;
            Some(BackendEvent::ContactUpsert(patch))
        }
        ConvKey::Group(_) => {
            let mut patch = ConversationPatch::new(conv.clone());
            patch.avatar = avatar;
            Some(BackendEvent::ConversationUpsert(patch))
        }
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use libthreema::protobuf::common::{Blob, Image, delta_image};

    use super::*;

    fn updated(id: u8, key: Option<u8>) -> Option<DeltaImage> {
        Some(DeltaImage {
            image: Some(delta_image::Image::Updated(Image {
                r#type: 0,
                blob: Some(Blob {
                    id: vec![id; ID_LEN],
                    nonce: Vec::new(),
                    key: key.map(|k| vec![k; KEY_LEN]).unwrap_or_default(),
                    uploaded_at: 1,
                }),
            })),
        })
    }

    fn removed() -> Option<DeltaImage> {
        Some(DeltaImage {
            image: Some(delta_image::Image::Removed(common::Unit {})),
        })
    }

    fn contact(user_set: Option<DeltaImage>, own: Option<DeltaImage>) -> d2d_sync::Contact {
        d2d_sync::Contact {
            identity: "ECHOECHO".to_owned(),
            user_defined_profile_picture: user_set,
            contact_defined_profile_picture: own,
            ..d2d_sync::Contact::default()
        }
    }

    #[test]
    fn the_users_choice_goes_before_the_contacts_own() {
        let both = contact(updated(1, Some(9)), updated(2, Some(9)));
        assert_eq!(
            picture_of_contact(&both).expect("a picture").id,
            [1; ID_LEN]
        );
        let own = contact(removed(), updated(2, Some(9)));
        assert_eq!(picture_of_contact(&own).expect("a picture").id, [2; ID_LEN]);
        assert!(picture_of_contact(&contact(removed(), removed())).is_none());
        assert!(picture_of_contact(&contact(None, None)).is_none());
    }

    #[test]
    fn a_blob_without_a_nonce_has_the_file_nonce() {
        let named = named(updated(1, Some(9)).as_ref()).expect("a picture");
        assert_eq!(named.nonce, FILE_NONCE);
        assert_eq!(named.key, Some([9; KEY_LEN]));
    }

    fn snapshot(contacts: Vec<d2d_sync::Contact>) -> Snapshot {
        Snapshot {
            own_id: "KOURIER3".to_owned(),
            profile: None,
            settings: None,
            contacts,
            groups: Vec::new(),
            lists: Vec::new(),
        }
    }

    #[test]
    fn only_what_differs_is_due() {
        let conv = ConvKey::Direct(user("ECHOECHO").expect("an id")).encode();
        let shot = snapshot(vec![contact(updated(1, Some(9)), None)]);
        // Never looked at.
        assert_eq!(differing(&shot, &HashMap::new()).len(), 1);
        // The same blob.
        let same = HashMap::from([(conv.clone(), Some(vec![1; ID_LEN]))]);
        assert!(differing(&shot, &same).is_empty());
        // Another blob.
        let other = HashMap::from([(conv.clone(), Some(vec![2; ID_LEN]))]);
        assert_eq!(differing(&shot, &other).len(), 1);
        // No picture, none known.
        let without = snapshot(vec![contact(None, None)]);
        assert!(differing(&without, &HashMap::new()).is_empty());
        // A picture known and gone from the sync.
        let gone = differing(&without, &same);
        assert_eq!(gone.len(), 1);
        assert!(gone[0].named.is_none());
    }

    #[test]
    fn a_picture_from_a_join_is_taken_as_it_is_and_one_from_the_mirror_is_opened() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2];
        let named = Named {
            id: [1; ID_LEN],
            key: Some([9; KEY_LEN]),
            nonce: FILE_NONCE,
        };
        assert_eq!(plain(&jpeg, &named).as_deref(), Some(&jpeg[..]));
        let boxed = crate::blob::tests::sealed(&[9; KEY_LEN], &FILE_NONCE, &jpeg);
        assert_eq!(plain(&boxed, &named).as_deref(), Some(&jpeg[..]));
        let not_a_jpeg = crate::blob::tests::sealed(&[9; KEY_LEN], &FILE_NONCE, b"<svg>");
        assert!(plain(&not_a_jpeg, &named).is_none());
    }

    #[test]
    fn a_person_is_a_contact_and_a_group_a_chat() {
        let person = ConvKey::Direct(user("ECHOECHO").expect("an id"));
        assert!(matches!(
            statement_of(&person, Patch::Clear),
            Some(BackendEvent::ContactUpsert(_))
        ));
        let group = group_key("ECHOECHO", 7).expect("a key");
        assert!(matches!(
            statement_of(&group, Patch::Clear),
            Some(BackendEvent::ConversationUpsert(_))
        ));
        assert!(statement_of(&group, Patch::Unchanged).is_none());
    }
}
