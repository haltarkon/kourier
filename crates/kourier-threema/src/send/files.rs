//! THE FILE OF A MESSAGE (TH8): what a `Transmit` of a file does before PREPARE.
//!
//! The steps are Threema Desktop's (`model/message/common.ts` `uploadBlobs`, `task/csp/common.ts` `getFileJsonData`): a
//! fresh random key per message, the file boxed under the constant file nonce and its thumbnail under the constant
//! thumbnail nonce (safe only because the key is never used again: a retry that finds no ledger row uploads anew under a
//! new key), both uploaded, and a file message's JSON naming the blobs. That JSON then goes through PREPARE as any
//! other body: the upload is what comes BEFORE the point of no return, so a failed upload sends nothing to anyone.
//!
//! A message carries one file: the core sends each attachment as a message of its own (`file_packing`'s default).
//!
//! Counts and kinds only in the log: no name, no id, no key.

use std::io::Read as _;
use std::path::{Component, Path};

use kourier_backend_api::{OutgoingKind, OutgoingRequest};
use kourier_blob::{BlobReader, FileKey};
use kourier_model::{Attachment, AttachmentKind, BackendError, BlobRef, Body, ErrorKind, Text};
use libthreema::common::ThreemaId;
use libthreema::protobuf::common::CspE2eMessageType;
use serde_json::{Map, Value, json};
use zeroize::Zeroizing;

use super::attempt::to_of;
use super::body::group_member_container;
use super::prepare::{NewBody, To};
use crate::blob::{self, FILE_NONCE, ID_LEN, KEY_LEN, MAX_FILE, Mirror, NONCE_LEN, THUMB_NONCE};

const NOT_HERE: &str = "the file to send is not here";
const NOT_SEALED: &str = "the file to send could not be boxed";
const NOT_BUILT: &str = "this kind of message is not sent yet";
const OVER: &str = "the file is larger than Threema carries";

/// The one file of `req`, with its caption, when that is what it sends. A quote or a timer has no writer here, and a
/// caption with ranges would lose them: such a message is the refusal of a plain `Transmit`.
pub(crate) fn file_of(req: &OutgoingRequest) -> Option<(&Attachment, Option<&Text>)> {
    let OutgoingKind::Message {
        body: Body::Media {
            attachments,
            caption,
        },
        reply_to: None,
        expiry: None,
    } = &req.kind
    else {
        return None;
    };
    let [attachment] = attachments.as_slice() else {
        return None;
    };
    let caption = attachment.caption.as_ref().or(caption.as_ref());
    if caption.is_some_and(|text| !text.ranges.is_empty()) {
        return None;
    }
    Some((attachment, caption))
}

/// The file of `req` uploaded, and the body that names it. Nothing is written to the send queue here.
pub(crate) async fn message_of(
    mirror: &Mirror,
    blob_dir: &Path,
    req: &OutgoingRequest,
) -> Result<NewBody, BackendError> {
    let (attachment, caption) =
        file_of(req).ok_or_else(|| BackendError::new(ErrorKind::Unsupported, NOT_BUILT))?;
    let to =
        to_of(&req.conv).ok_or_else(|| BackendError::new(ErrorKind::Unsupported, NOT_BUILT))?;
    let local = attachment
        .local
        .clone()
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, NOT_HERE))?;
    let thumb = attachment.thumb.clone();
    let key: Zeroizing<[u8; KEY_LEN]> = Zeroizing::new(rand::random());

    let (dir, sealing) = (blob_dir.to_path_buf(), key.clone());
    let boxed = tokio::task::spawn_blocking(move || {
        let file = boxed_file(&dir, &local, &sealing, &FILE_NONCE)?;
        let thumb = match thumb {
            // A thumbnail is a nicety and has to be a JPEG (what the receivers read): one that is not stays home.
            Some(thumb) => boxed_jpeg(&dir, &thumb, &sealing, &THUMB_NONCE),
            None => None,
        };
        Ok::<_, BackendError>((file, thumb))
    })
    .await
    .map_err(|_| BackendError::internal(NOT_SEALED))??;
    let (Boxed { bytes, plain_len }, thumb) = boxed;

    // Desktop's `getBlobUploadScope`: `public`, and persistent for a group, whose members may fetch it for longer.
    let persist = matches!(to, To::Group { .. });
    let blob = mirror.upload(bytes, persist).await?;
    let thumb = match thumb {
        Some(thumb) => Some(mirror.upload(thumb.bytes, persist).await?),
        None => None,
    };
    let inner = serde_json::to_vec(&description(
        attachment,
        &key,
        &blob,
        thumb.as_ref(),
        caption,
        plain_len,
    ))
    .map_err(|_| BackendError::internal(NOT_SEALED))?;
    wrapped(&to, inner)
}

/// The body of a file message to `to`: the JSON in a 1:1 chat, the member container around it in a group.
fn wrapped(to: &To, inner: Vec<u8>) -> Result<NewBody, BackendError> {
    Ok(match to {
        To::Contact(_) => NewBody::Raw {
            ty: CspE2eMessageType::File as u8,
            body: inner,
            distributes: true,
        },
        To::Group { creator, group_id } => {
            let creator = ThreemaId::try_from(creator.as_str())
                .map_err(|_| BackendError::new(ErrorKind::InvalidInput, NOT_BUILT))?;
            NewBody::Raw {
                ty: CspE2eMessageType::GroupFile as u8,
                body: group_member_container(creator, *group_id, &inner),
                distributes: true,
            }
        }
    })
}

/// A box and the length of what is in it.
struct Boxed {
    bytes: Vec<u8>,
    plain_len: u64,
}

fn boxed_file(
    dir: &Path,
    file: &BlobRef,
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Result<Boxed, BackendError> {
    let plain = plain_of(dir, file)?;
    if plain.len() > MAX_FILE {
        return Err(BackendError::new(ErrorKind::TooLarge, OVER));
    }
    let bytes = blob::seal(key, nonce, &plain).ok_or_else(|| BackendError::internal(NOT_SEALED))?;
    Ok(Boxed {
        bytes,
        plain_len: plain.len() as u64,
    })
}

fn boxed_jpeg(
    dir: &Path,
    file: &BlobRef,
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
) -> Option<Boxed> {
    let plain = plain_of(dir, file).ok()?;
    if !plain.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return None;
    }
    let bytes = blob::seal(key, nonce, &plain)?;
    Some(Boxed {
        bytes,
        plain_len: plain.len() as u64,
    })
}

/// A file of the core's, opened under its key.
fn plain_of(dir: &Path, file: &BlobRef) -> Result<Zeroizing<Vec<u8>>, BackendError> {
    let inside = !file.path.as_os_str().is_empty()
        && file
            .path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !inside {
        return Err(BackendError::new(ErrorKind::InvalidInput, NOT_HERE));
    }
    let key = FileKey::from_bytes(*file.key.expose());
    let unreadable = || BackendError::new(ErrorKind::InvalidInput, NOT_HERE);
    let mut reader = BlobReader::open(&dir.join(&file.path), &key).map_err(|_| unreadable())?;
    let mut plain = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plain).map_err(|_| unreadable())?;
    Ok(plain)
}

/// The JSON of a file message (`getFileJsonData`), with both rendering fields.
fn description(
    attachment: &Attachment,
    key: &[u8; KEY_LEN],
    blob: &[u8; ID_LEN],
    thumb: Option<&[u8; ID_LEN]>,
    caption: Option<&Text>,
    size: u64,
) -> Value {
    let mut object = Map::new();
    let mut metadata = Map::new();
    let seconds = attachment.duration_ms.map(|ms| f64::from(ms) / 1000.0);
    let rendering = match attachment.kind {
        AttachmentKind::File => 0,
        AttachmentKind::Image | AttachmentKind::Gif => {
            if attachment.kind == AttachmentKind::Gif {
                metadata.insert("a".to_owned(), json!(true));
            }
            sides(&mut metadata, attachment.dims);
            1
        }
        AttachmentKind::Video => {
            duration(&mut metadata, seconds);
            sides(&mut metadata, attachment.dims);
            1
        }
        AttachmentKind::Audio | AttachmentKind::Voice => {
            duration(&mut metadata, seconds);
            1
        }
    };
    object.insert("j".to_owned(), json!(rendering));
    object.insert("i".to_owned(), json!(rendering));
    object.insert("k".to_owned(), json!(blob::hex(key)));
    object.insert("b".to_owned(), json!(blob::hex(blob)));
    object.insert("m".to_owned(), json!(attachment.mime));
    if let Some(name) = attachment.file_name.as_deref() {
        object.insert("n".to_owned(), json!(name));
    }
    object.insert("s".to_owned(), json!(size));
    if let Some(thumb) = thumb {
        object.insert("t".to_owned(), json!(blob::hex(thumb)));
        object.insert("p".to_owned(), json!("image/jpeg"));
    }
    if let Some(caption) = caption.filter(|text| !text.body.is_empty()) {
        object.insert("d".to_owned(), json!(caption.body));
    }
    if !metadata.is_empty() {
        object.insert("x".to_owned(), Value::Object(metadata));
    }
    Value::Object(object)
}

fn sides(metadata: &mut Map<String, Value>, dims: Option<(u32, u32)>) {
    if let Some((width, height)) = dims {
        metadata.insert("w".to_owned(), json!(width));
        metadata.insert("h".to_owned(), json!(height));
    }
}

fn duration(metadata: &mut Map<String, Value>, seconds: Option<f64>) {
    if let Some(seconds) = seconds {
        metadata.insert("d".to_owned(), json!(seconds));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kourier_backend_api::{Clock, FakeClock};
    use kourier_blob::BlobStore;
    use kourier_model::{AttachmentState, BlobKey, ConvKey};
    use tempfile::TempDir;

    use super::*;
    use crate::blob::tests::{FakeMirror, mirror_of};
    use crate::join::events::user;
    use crate::reflect::body;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 7, 7, 7];

    fn written(dir: &Path, plain: &[u8]) -> BlobRef {
        let store = BlobStore::open(dir).expect("a store");
        let id = kourier_blob::BlobId::new_random();
        let key = FileKey::generate();
        let mut writer = store.create(&id, &key).expect("a writer");
        std::io::Write::write_all(&mut writer, plain).expect("written");
        writer.finish().expect("finished");
        BlobRef {
            path: id.relative_path(),
            key: BlobKey::new(*key.as_bytes()),
        }
    }

    fn attachment(kind: AttachmentKind, local: Option<BlobRef>) -> Attachment {
        Attachment {
            id: None,
            kind,
            mime: "image/jpeg".to_owned(),
            size: 7,
            file_name: Some("a.jpg".to_owned()),
            dims: Some((640, 480)),
            duration_ms: None,
            blurhash: None,
            waveform: None,
            caption: None,
            state: AttachmentState::default(),
            remote: None,
            local,
            thumb: None,
            long_text: false,
            net_ids: Vec::new(),
        }
    }

    fn request(
        conv: ConvKey,
        attachments: Vec<Attachment>,
        caption: Option<&str>,
    ) -> OutgoingRequest {
        OutgoingRequest {
            conv,
            local: Arc::from("0102030405060708"),
            sent_at: 1,
            kind: OutgoingKind::Message {
                body: Body::Media {
                    attachments,
                    caption: caption.map(|body| Text {
                        body: body.to_owned(),
                        ranges: Vec::new(),
                    }),
                },
                reply_to: None,
                expiry: None,
            },
            only: None,
            is_recipient_update: false,
        }
    }

    fn direct() -> ConvKey {
        ConvKey::Direct(user("ECHOECHO").expect("an id"))
    }

    fn fixture() -> (TempDir, Arc<FakeMirror>, Mirror) {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(1));
        let https = Arc::new(FakeMirror::default());
        let mirror = mirror_of(https.clone(), clock);
        (TempDir::new().expect("a dir"), https, mirror)
    }

    fn raw_of(body: NewBody) -> (u8, Vec<u8>) {
        match body {
            NewBody::Raw { ty, body, .. } => (ty, body),
            NewBody::Text(_) => panic!("a text"),
        }
    }

    #[tokio::test]
    async fn a_file_goes_up_boxed_and_its_message_reads_back_as_the_receive_path_reads_it() {
        let (dir, https, mirror) = fixture();
        let local = written(dir.path(), JPEG);
        let req = request(
            direct(),
            vec![attachment(AttachmentKind::Image, Some(local))],
            Some("look"),
        );
        let (ty, json) = raw_of(message_of(&mirror, dir.path(), &req).await.expect("a body"));
        assert_eq!(ty, CspE2eMessageType::File as u8);

        let parsed = body::file(&json, "", false).expect("the receive path reads it");
        assert_eq!(parsed.attachment.kind, AttachmentKind::Image);
        assert_eq!(parsed.attachment.size, JPEG.len() as u64);
        assert_eq!(parsed.attachment.dims, Some((640, 480)));

        let value: Value = serde_json::from_slice(&json).expect("JSON");
        assert_eq!(value["d"], "look");
        assert_eq!(value["j"], 1);
        assert_eq!(value["i"], 1);
        // What went up opens under the key the message names, and is the file.
        let key: [u8; KEY_LEN] = blob::tests::unhexed(value["k"].as_str().expect("a key"));
        let uploaded = https.uploaded();
        assert_eq!(uploaded.len(), 1);
        assert_eq!(
            blob::open(&key, &FILE_NONCE, &uploaded[0]).as_deref(),
            Some(JPEG)
        );
        let urls = https.urls();
        assert!(
            urls[0].contains("upload?persist=0&deviceId="),
            "{}",
            urls[0]
        );
        assert!(urls[0].ends_with("&scope=public"), "{}", urls[0]);
    }

    #[tokio::test]
    async fn a_groups_file_persists_and_sits_in_the_member_container() {
        let (dir, https, mirror) = fixture();
        let local = written(dir.path(), JPEG);
        let group = crate::join::events::group_key("ECHOECHO", 7).expect("a key");
        let req = request(
            group,
            vec![attachment(AttachmentKind::File, Some(local))],
            None,
        );
        let (ty, wrapped) = raw_of(message_of(&mirror, dir.path(), &req).await.expect("a body"));
        assert_eq!(ty, CspE2eMessageType::GroupFile as u8);
        assert_eq!(&wrapped[..8], b"ECHOECHO");
        assert_eq!(&wrapped[8..16], &7_u64.to_le_bytes());
        let value: Value = serde_json::from_slice(&wrapped[16..]).expect("JSON");
        assert_eq!(value["j"], 0, "a file renders as a file");
        assert!(https.urls()[0].contains("upload?persist=1&"), "persistent");
    }

    #[tokio::test]
    async fn a_thumbnail_goes_up_under_its_own_nonce_and_one_that_is_no_jpeg_stays_home() {
        let (dir, https, mirror) = fixture();
        let local = written(dir.path(), JPEG);
        let thumb = written(dir.path(), JPEG);
        let mut with = attachment(AttachmentKind::Image, Some(local.clone()));
        with.thumb = Some(thumb);
        let req = request(direct(), vec![with], None);
        let (_, json) = raw_of(message_of(&mirror, dir.path(), &req).await.expect("a body"));
        let value: Value = serde_json::from_slice(&json).expect("JSON");
        let key: [u8; KEY_LEN] = blob::tests::unhexed(value["k"].as_str().expect("a key"));
        assert_eq!(value["p"], "image/jpeg");
        let uploaded = https.uploaded();
        assert_eq!(uploaded.len(), 2, "the file and the thumbnail");
        assert_eq!(
            blob::open(&key, &THUMB_NONCE, &uploaded[1]).as_deref(),
            Some(JPEG)
        );

        let (dir, https, mirror) = fixture();
        let local = written(dir.path(), JPEG);
        let png = written(dir.path(), b"\x89PNG\r\n");
        let mut with = attachment(AttachmentKind::Image, Some(local));
        with.thumb = Some(png);
        let req = request(direct(), vec![with], None);
        let (_, json) = raw_of(message_of(&mirror, dir.path(), &req).await.expect("a body"));
        let value: Value = serde_json::from_slice(&json).expect("JSON");
        assert!(value.get("t").is_none());
        assert_eq!(https.uploaded().len(), 1);
    }

    #[tokio::test]
    async fn every_message_has_its_own_key() {
        let (dir, _https, mirror) = fixture();
        let local = written(dir.path(), JPEG);
        let req = request(
            direct(),
            vec![attachment(AttachmentKind::Image, Some(local))],
            None,
        );
        let (_, one) = raw_of(message_of(&mirror, dir.path(), &req).await.expect("a body"));
        let (_, two) = raw_of(message_of(&mirror, dir.path(), &req).await.expect("a body"));
        let key = |json: &[u8]| -> String {
            let value: Value = serde_json::from_slice(json).expect("JSON");
            value["k"].as_str().expect("a key").to_owned()
        };
        assert_ne!(key(&one), key(&two));
    }

    #[tokio::test]
    async fn a_refused_upload_sends_nothing_and_says_why() {
        let (dir, https, mirror) = fixture();
        let local = written(dir.path(), JPEG);
        let req = request(
            direct(),
            vec![attachment(AttachmentKind::Image, Some(local))],
            None,
        );
        https.upload_answers(413);
        let error = message_of(&mirror, dir.path(), &req)
            .await
            .expect_err("refused");
        assert_eq!(error.kind, ErrorKind::TooLarge);
        https.upload_answers(503);
        let error = message_of(&mirror, dir.path(), &req)
            .await
            .expect_err("refused");
        assert!(
            matches!(error.kind, ErrorKind::Transient { .. }),
            "{:?}",
            error.kind
        );
    }

    #[test]
    fn what_this_step_does_not_write_is_no_file_of_it() {
        let two = request(
            direct(),
            vec![
                attachment(AttachmentKind::Image, None),
                attachment(AttachmentKind::Image, None),
            ],
            None,
        );
        assert!(file_of(&two).is_none(), "two files in one message");
        let one = request(
            direct(),
            vec![attachment(AttachmentKind::Image, None)],
            None,
        );
        assert!(file_of(&one).is_some());
        let mut timed = one.clone();
        if let OutgoingKind::Message { expiry, .. } = &mut timed.kind {
            *expiry = Some(std::time::Duration::from_secs(1));
        }
        assert!(file_of(&timed).is_none(), "a timer");
    }

    #[tokio::test]
    async fn a_file_that_is_not_here_goes_nowhere() {
        let (dir, https, mirror) = fixture();
        let req = request(
            direct(),
            vec![attachment(AttachmentKind::Image, None)],
            None,
        );
        let error = message_of(&mirror, dir.path(), &req)
            .await
            .expect_err("no file");
        assert_eq!(error.kind, ErrorKind::InvalidInput);
        assert!(https.urls().is_empty());
    }
}
