//! Synthetic attachments (§10.2): the fake's network pointers and its "CDN". A pointer names
//! what the download produces (a deterministic picture from [`kourier_media::sample_image`], or
//! deterministic bytes) or how it fails; `DownloadAttachment` streams the plaintext into the
//! core's `BlobTarget` like a real backend, and outgoing attachments are read back from the
//! core's blobs before a transmit is accepted.

use std::path::Path;

use kourier_backend_api::BlobTarget;
use kourier_blob::FileKey;
use kourier_media::SampleFormat;
use kourier_model::{
    Attachment, AttachmentKind, AttachmentMeta, BackendBlob, BackendError, BlobRef, ErrorKind,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// `BackendBlob::v` of the fake's pointers.
pub const POINTER_VERSION: u16 = 0x46;

/// What a fake pointer downloads to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pointer {
    /// A JPEG picture.
    Image {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// Picture seed.
        seed: u64,
    },
    /// `len` pseudo-random bytes.
    Bytes {
        /// Length.
        len: u64,
        /// Content seed.
        seed: u64,
    },
    /// A file with exactly these bytes (the 3D model fixtures).
    Data {
        /// The content.
        bytes: Vec<u8>,
    },
    /// A text file with exactly this content (the whole text of a long message).
    Text {
        /// The content.
        text: String,
    },
    /// The CDN no longer has it (`AttachmentDone` with `ErrorKind::Expired`).
    Expired,
}

impl Pointer {
    /// As an attachment's `remote`.
    pub fn to_blob(&self) -> BackendBlob {
        BackendBlob::new(
            POINTER_VERSION,
            serde_json::to_vec(self).unwrap_or_default(),
        )
    }

    /// Parses a fake pointer.
    pub fn from_blob(b: &BackendBlob) -> Option<Self> {
        (b.v == POINTER_VERSION)
            .then(|| serde_json::from_slice(&b.bytes).ok())
            .flatten()
    }

    /// The plaintext the download writes (`None` for a failing pointer).
    pub fn plaintext(&self) -> Option<Result<Vec<u8>, BackendError>> {
        match self {
            Self::Image {
                width,
                height,
                seed,
            } => Some(
                kourier_media::sample_image(*width, *height, *seed, SampleFormat::Jpeg)
                    .map_err(|e| BackendError::internal(format!("fake image: {e}"))),
            ),
            Self::Bytes { len, seed } => Some(Ok(bytes(*len, *seed))),
            Self::Data { bytes } => Some(Ok(bytes.clone())),
            Self::Text { text } => Some(Ok(text.clone().into_bytes())),
            Self::Expired => None,
        }
    }
}

fn bytes(len: u64, seed: u64) -> Vec<u8> {
    let mut x = seed ^ 0x2545_f491_4f6c_dd1d;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            u8::try_from(x & 0xff).unwrap_or(0)
        })
        .collect()
}

/// An incoming picture attachment whose download yields a `width` × `height` JPEG (the size is
/// an estimate; `AttachmentDone` reports the real one).
pub fn synthetic_image(width: u32, height: u32, seed: u64) -> Attachment {
    Attachment {
        kind: AttachmentKind::Image,
        mime: "image/jpeg".into(),
        size: u64::from(width) * u64::from(height) / 8,
        dims: Some((width, height)),
        remote: Some(
            Pointer::Image {
                width,
                height,
                seed,
            }
            .to_blob(),
        ),
        ..Attachment::default()
    }
}

/// A picture whose sender claims `claim` (width, height) but whose file is `width` × `height`:
/// a phone that ignores the EXIF rotation, or a size that is only a guess. The row is laid out
/// by the claim until the download reports the real size, and then its height changes.
pub fn synthetic_image_claiming(
    claim: (u32, u32),
    width: u32,
    height: u32,
    seed: u64,
) -> Attachment {
    Attachment {
        dims: Some(claim),
        ..synthetic_image(width, height, seed)
    }
}

/// An incoming file attachment of `len` deterministic bytes.
pub fn synthetic_file(name: &str, mime: &str, len: u64, seed: u64) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: mime.into(),
        size: len,
        file_name: Some(name.into()),
        remote: Some(Pointer::Bytes { len, seed }.to_blob()),
        ..Attachment::default()
    }
}

/// A file attachment with exactly `bytes` as its download.
fn data_file(name: &str, mime: &str, bytes: Vec<u8>) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: mime.into(),
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        file_name: Some(name.into()),
        remote: Some(Pointer::Data { bytes }.to_blob()),
        ..Attachment::default()
    }
}

/// The 12 triangles of the box `lo` … `hi` (counter-clockwise seen from outside).
fn box_triangles(lo: [f32; 3], hi: [f32; 3]) -> Vec<[f32; 9]> {
    let p = |i: usize| -> [f32; 3] {
        [
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        ]
    };
    let quads = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    let mut out = Vec::new();
    for q in quads {
        for tri in [[q[0], q[1], q[2]], [q[0], q[2], q[3]]] {
            let mut t = [0f32; 9];
            for (k, &i) in tri.iter().enumerate() {
                t[3 * k..3 * k + 3].copy_from_slice(&p(i));
            }
            out.push(t);
        }
    }
    out
}

/// A small binary STL (a 60 x 31 x 48 mm angle bracket with a block on its plate: three
/// overlapping boxes, which is all a still needs), built here, never a committed file.
pub fn synthetic_stl(name: &str) -> Attachment {
    let mut tris = box_triangles([0.0, 0.0, 0.0], [60.0, 31.0, 6.0]);
    tris.extend(box_triangles([0.0, 0.0, 0.0], [60.0, 6.0, 48.0]));
    tris.extend(box_triangles([22.0, 12.0, 6.0], [38.0, 26.0, 20.0]));
    let mut bytes = vec![0u8; 80];
    bytes.extend_from_slice(&u32::try_from(tris.len()).unwrap_or(0).to_le_bytes());
    for t in &tris {
        bytes.extend_from_slice(&[0u8; 12]);
        for c in t {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        bytes.extend_from_slice(&[0u8; 2]);
    }
    data_file(name, "model/stl", bytes)
}

/// A lumpy sphere of about `2 * segments * (segments / 2 - 1)` triangles as a binary STL (for the
/// viewer's real-GL proof on the PC: 320 gives ~100 k triangles), built here, never a committed file.
pub fn synthetic_sphere_stl(name: &str, segments: usize) -> Attachment {
    let rings = (segments / 2).max(3);
    let vertex = |i: usize, j: usize| -> [f32; 3] {
        let theta = std::f64::consts::PI * j as f64 / rings as f64;
        let phi = 2.0 * std::f64::consts::PI * (i % segments) as f64 / segments as f64;
        let r = 40.0 * (1.0 + 0.08 * (5.0 * phi).sin() * (3.0 * theta).cos());
        [
            (r * theta.sin() * phi.cos()) as f32,
            (r * theta.sin() * phi.sin()) as f32,
            (r * theta.cos()) as f32,
        ]
    };
    let mut tris: Vec<[f32; 9]> = Vec::new();
    let mut push = |a: [f32; 3], b: [f32; 3], c: [f32; 3]| {
        let mut t = [0f32; 9];
        t[0..3].copy_from_slice(&a);
        t[3..6].copy_from_slice(&b);
        t[6..9].copy_from_slice(&c);
        tris.push(t);
    };
    for j in 0..rings {
        for i in 0..segments {
            let (a, b, c, d) = (
                vertex(i, j),
                vertex(i + 1, j),
                vertex(i + 1, j + 1),
                vertex(i, j + 1),
            );
            if j > 0 {
                push(a, d, b);
            }
            if j + 1 < rings {
                push(b, d, c);
            }
        }
    }
    let mut bytes = vec![0u8; 80];
    bytes.extend_from_slice(&u32::try_from(tris.len()).unwrap_or(0).to_le_bytes());
    for t in &tris {
        bytes.extend_from_slice(&[0u8; 12]);
        for c in t {
            bytes.extend_from_slice(&c.to_le_bytes());
        }
        bytes.extend_from_slice(&[0u8; 2]);
    }
    data_file(name, "model/stl", bytes)
}

/// A small 3MF with its own colours (standard `basematerials`, one colour per face pair of a
/// 40 mm cube and a 20 mm block on top), built here as a ZIP, never a committed file.
pub fn synthetic_3mf(name: &str) -> Attachment {
    use std::io::Write;
    let colours = [
        "#D9442B", "#2B7BD9", "#3DAA55", "#E8B923", "#8E44AD", "#16A5A5",
    ];
    let bases: String = colours
        .iter()
        .enumerate()
        .map(|(i, c)| format!(r#"<base name="c{i}" displaycolor="{c}"/>"#))
        .collect();
    let mut vertices = String::new();
    let mut triangles = String::new();
    let mut corner = 0usize;
    for (lo, hi) in [
        ([0.0f32, 0.0, 0.0], [40.0f32, 40.0, 40.0]),
        ([10.0, 10.0, 40.0], [30.0, 30.0, 60.0]),
    ] {
        let tris = box_triangles(lo, hi);
        for (n, t) in tris.iter().enumerate() {
            for k in 0..3 {
                vertices.push_str(&format!(
                    r#"<vertex x="{}" y="{}" z="{}"/>"#,
                    t[3 * k],
                    t[3 * k + 1],
                    t[3 * k + 2]
                ));
            }
            triangles.push_str(&format!(
                r#"<triangle v1="{}" v2="{}" v3="{}" pid="1" p1="{}"/>"#,
                corner,
                corner + 1,
                corner + 2,
                (n / 2) % colours.len()
            ));
            corner += 3;
        }
    }
    let model = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources><basematerials id="1">{bases}</basematerials><object id="2" type="model"><mesh><vertices>{vertices}</vertices><triangles>{triangles}</triangles></mesh></object></resources><build><item objectid="2"/></build></model>"#
    );
    let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/></Relationships>"#;
    let types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/></Types>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (part, body) in [
        ("[Content_Types].xml", types),
        ("_rels/.rels", rels),
        ("3D/3dmodel.model", model.as_str()),
    ] {
        zip.start_file(part, options).expect("zip part");
        zip.write_all(body.as_bytes()).expect("zip body");
    }
    let bytes = zip.finish().expect("zip end").into_inner();
    data_file(name, "model/3mf", bytes)
}

/// The whole text of a message whose body arrives trimmed: an attachment marked
/// `Attachment::long_text` whose download yields `text` (what a backend emits when it could
/// not fetch the long text itself). The core fetches it and joins it into the body.
pub fn synthetic_long_text(text: &str) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: "text/x-fake-long-text".into(),
        size: u64::try_from(text.len()).unwrap_or(u64::MAX),
        remote: Some(Pointer::Text { text: text.into() }.to_blob()),
        long_text: true,
        ..Attachment::default()
    }
}

/// An incoming attachment the CDN no longer has.
pub fn expired_attachment(kind: AttachmentKind) -> Attachment {
    Attachment {
        kind,
        mime: "image/jpeg".into(),
        size: 1000,
        remote: Some(Pointer::Expired.to_blob()),
        ..Attachment::default()
    }
}

/// Downloads `remote` into `dest` (blocking): the plaintext goes through `kourier-blob`'s writer
/// with the core's key. Returns what `AttachmentDone` reports.
pub(crate) fn download(
    remote: &BackendBlob,
    dest: &BlobTarget,
) -> Result<AttachmentMeta, BackendError> {
    let pointer = Pointer::from_blob(remote)
        .ok_or_else(|| BackendError::unsupported("not a fake attachment pointer"))?;
    let plain = match pointer.plaintext() {
        Some(p) => Zeroizing::new(p?),
        None => {
            return Err(BackendError::new(
                ErrorKind::Expired,
                "gone from the fake CDN",
            ));
        }
    };
    let key = FileKey::from_bytes(*dest.key.expose());
    let size = kourier_blob::write_all(&dest.path, &key, &plain)
        .map_err(|e| BackendError::internal(format!("fake download: {e}")))?;
    let (mime, dims) = match pointer {
        Pointer::Image { width, height, .. } => {
            (Some("image/jpeg".to_owned()), Some((width, height)))
        }
        _ => (None, None),
    };
    Ok(AttachmentMeta {
        size,
        mime,
        dims,
        duration_ms: None,
    })
}

/// Reads an outgoing attachment's blob completely, as a real backend does before it uploads
/// (blocking). The size must match the attachment's.
pub(crate) fn read_outgoing(blob_dir: &Path, a: &Attachment) -> Result<u64, BackendError> {
    let BlobRef { path, key } = a
        .local
        .as_ref()
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "attachment without a file"))?;
    let plain = kourier_blob::read_all(&blob_dir.join(path), &FileKey::from_bytes(*key.expose()))
        .map_err(|e| {
        BackendError::new(
            ErrorKind::InvalidInput,
            format!("attachment unreadable: {e}"),
        )
    })?;
    let len = u64::try_from(plain.len()).unwrap_or(u64::MAX);
    if len != a.size {
        return Err(BackendError::new(
            ErrorKind::InvalidInput,
            "attachment size does not match its file",
        ));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::BlobKey;

    #[test]
    fn a_claim_that_differs_from_the_file_is_corrected_by_the_download() {
        let a = synthetic_image_claiming((200, 90), 90, 200, 7);
        assert_eq!(a.dims, Some((200, 90)), "the row is laid out by the claim");
        let remote = a.remote.clone().expect("remote");
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = BlobTarget {
            path: dir.path().join("blob"),
            key: BlobKey::new([3; 32]),
        };
        let meta = download(&remote, &dest).expect("download");
        assert_eq!(
            meta.dims,
            Some((90, 200)),
            "the download reports the real size"
        );
    }

    #[test]
    fn pointers_round_trip_and_download_into_the_target() {
        let a = synthetic_image(64, 48, 9);
        let remote = a.remote.clone().expect("remote");
        let p = Pointer::from_blob(&remote).expect("pointer");
        assert_eq!(
            p,
            Pointer::Image {
                width: 64,
                height: 48,
                seed: 9
            }
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = BlobTarget {
            path: dir.path().join("blob"),
            key: BlobKey::new([3; 32]),
        };
        let meta = download(&remote, &dest).expect("download");
        let plain =
            kourier_blob::read_all(&dest.path, &FileKey::from_bytes([3; 32])).expect("read");
        assert_eq!(meta.size, plain.len() as u64);
        assert_eq!(*plain, p.plaintext().expect("bytes").expect("ok"));
        assert_eq!(meta.dims, Some((64, 48)));

        let file = synthetic_file("a.bin", "application/octet-stream", 100, 1);
        let dest2 = BlobTarget {
            path: dir.path().join("blob2"),
            key: BlobKey::new([4; 32]),
        };
        assert_eq!(
            download(file.remote.as_ref().expect("remote"), &dest2)
                .expect("file")
                .size,
            100
        );
        let gone = expired_attachment(AttachmentKind::Image);
        let err = download(gone.remote.as_ref().expect("remote"), &dest2).expect_err("expired");
        assert_eq!(err.kind, ErrorKind::Expired);
        let err = download(&BackendBlob::new(1, vec![1]), &dest2).expect_err("foreign");
        assert_eq!(err.kind, ErrorKind::Unsupported);
    }
}
