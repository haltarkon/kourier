//! The streaming reader of a transfer archive. See the module docs of `crate::archive`.

use std::fmt;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::executor::block_on;
use futures::io::{AsyncRead, AsyncReadExt as _, BufReader};
use libsignal_message_backup::frame::{
    FileReaderFactory, FramesReader, ReaderFactory, ValidationError, VerifyHmac as _,
    VerifyHmacError,
};
use prost::Message as _;
use zeroize::Zeroizing;

use super::{ArchiveError, ArchiveInfo, ArchiveKey, Frame, Limits, SUPPORTED_VERSION};
use crate::proto::backup;

/// What libsignal reads the file through (blocking `std::fs::File` behind `AllowStdIo`, so
/// every poll is ready: the reader is driven by `block_on` on the caller's thread).
type Source = <FileReaderFactory<&'static Path> as ReaderFactory>::Reader;

/// How a file of the OTHER backup format begins: a magic number, then an unencrypted metadata
/// header that no MAC covers (remote backups with a forward-secrecy token). A transfer
/// archive never has it (the phone writes it without that header), and libsignal parses the
/// header before any check, with a reader that allocates by a length it reads from the file:
/// 13 bytes could ask for 4 GiB, and a header put in front of a real archive was accepted.
const HEADER_FORMAT_MAGIC: &[u8; 8] = b"SBACKUP\x01";

/// Opens the archive file for libsignal's two passes and refuses the header format on each.
struct TransferFile<'a> {
    path: &'a Path,
}

impl ReaderFactory for TransferFile<'_> {
    type Reader = Source;

    fn make_reader(&mut self) -> io::Result<Self::Reader> {
        let mut head = [0u8; HEADER_FORMAT_MAGIC.len()];
        let mut file = std::fs::File::open(self.path)?;
        let mut got = 0;
        while got < head.len() {
            match io::Read::read(&mut file, &mut head[got..])? {
                0 => break,
                n => got += n,
            }
        }
        if got == head.len() && head == *HEADER_FORMAT_MAGIC {
            return Err(io::Error::other(HeaderFormat));
        }
        drop(file);
        // (The file is in a directory only this user writes: it is the same file.)
        FileReaderFactory { path: self.path }.make_reader()
    }
}

/// The error [`TransferFile`] raises, recognised by [`classify`].
#[derive(Debug)]
struct HeaderFormat;

impl fmt::Display for HeaderFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the file has the header of another backup format")
    }
}

impl std::error::Error for HeaderFormat {}

/// The decrypted, decompressed stream of frames.
type Decrypted = FramesReader<Source>;

/// The buffer between the gzip decoder and the length prefixes: a prefix is one or two bytes,
/// and every byte read straight from the decoder costs a pass through its state machine.
const BUFFER_BYTES: usize = 64 * 1024;

/// The most bytes allocated for a frame before any of it has been read: a length prefix can
/// claim up to `Limits::max_frame_bytes` while the stream has only a few bytes left.
const FIRST_ALLOCATION: usize = 1024 * 1024;

/// A length prefix is a varint of at most this many bytes (a `u32`).
const MAX_VARINT_BYTES: usize = 5;

/// Reads the frames of one archive file, one at a time. Not `Send`: open it and drain it on
/// the same (blocking) thread. `Debug` prints no content.
pub(crate) struct ArchiveReader {
    state: State,
    limits: Limits,
    info: ArchiveInfo,
    frames: u64,
    decompressed: u64,
}

enum State {
    Open(Box<BufReader<Capped<Decrypted>>>),
    Done,
    Failed(ArchiveError),
}

impl ArchiveReader {
    /// Opens the archive at `path`: checks the HMAC over the whole file (a pass that reads it
    /// all, before anything is decrypted), then reads and checks the `BackupInfo`.
    ///
    /// Errors: [`ArchiveError::Hmac`] (also a wrong key), `Truncated`, `Io`, `MissingInfo`,
    /// `UnsupportedVersion`, and the limits. Blocking: call it from a blocking thread.
    pub(crate) fn open(
        path: &Path,
        key: &ArchiveKey,
        limits: Limits,
    ) -> Result<Self, ArchiveError> {
        // The factory opens the path again for the second pass; it only borrows it.
        let frames = block_on(FramesReader::new(
            key.message_backup_key(),
            TransferFile { path },
        ))
        .map_err(from_validation)?;
        let mut input = BufReader::with_capacity(
            BUFFER_BYTES,
            Capped {
                inner: frames,
                read: 0,
                max: limits.max_decompressed_bytes,
            },
        );

        let Some(len) = block_on(read_length(&mut input))? else {
            return Err(ArchiveError::MissingInfo);
        };
        let len = check_length(len, &limits)?;
        let bytes = block_on(read_payload(&mut input, len))?;
        check_fields(&bytes, &limits)?;
        let info = ArchiveInfo::from_proto(
            backup::BackupInfo::decode(bytes.as_slice()).map_err(|_| ArchiveError::BadProtobuf)?,
        );
        if info.version != SUPPORTED_VERSION {
            return Err(ArchiveError::UnsupportedVersion {
                found: info.version,
            });
        }
        let decompressed = input.get_ref().read;
        Ok(Self {
            state: State::Open(Box::new(input)),
            limits,
            info,
            frames: 0,
            decompressed,
        })
    }

    /// The archive's `BackupInfo` (always version [`SUPPORTED_VERSION`]).
    pub(crate) fn info(&self) -> &ArchiveInfo {
        &self.info
    }

    /// The frames returned so far (the `BackupInfo` is not counted).
    #[cfg(test)]
    pub(crate) fn frames_read(&self) -> u64 {
        self.frames
    }

    /// The bytes the gzip stream has produced so far, `BackupInfo` and read-ahead included.
    #[cfg(test)]
    pub(crate) fn decompressed_bytes(&self) -> u64 {
        self.decompressed
    }

    /// The next frame, or `Ok(None)` at the end of the archive, after the final HMAC check.
    /// A frame of an unset or unknown kind is `Ok(Some(Frame::Unknown))`. After an error every
    /// call returns that error again. Blocking: call it from a blocking thread.
    pub(crate) fn next_frame(&mut self) -> Result<Option<Frame>, ArchiveError> {
        match &self.state {
            State::Done => return Ok(None),
            State::Failed(error) => return Err(*error),
            State::Open(_) => {}
        }
        let result = self.advance();
        if let Err(error) = &result {
            if let State::Open(input) = &self.state {
                self.decompressed = input.get_ref().read;
            }
            self.state = State::Failed(*error);
        }
        result
    }

    fn advance(&mut self) -> Result<Option<Frame>, ArchiveError> {
        let State::Open(input) = &mut self.state else {
            return Ok(None);
        };
        let Some(len) = block_on(read_length(input))? else {
            return self.finish();
        };
        // The cap on frames is checked when one more is known to exist, so exactly
        // `max_frames` frames are fine. The cap on the length is checked before the
        // allocation, and so before any of the frame is read.
        if self.frames >= self.limits.max_frames {
            return Err(ArchiveError::TooManyFrames {
                max: self.limits.max_frames,
            });
        }
        let len = check_length(len, &self.limits)?;
        let bytes = block_on(read_payload(input, len))?;
        self.decompressed = input.get_ref().read;
        self.frames += 1;
        // Before anything is decoded: what the frame will decode to is bounded by its
        // fields, not by its bytes.
        check_fields(&bytes, &self.limits)?;
        let frame =
            backup::Frame::decode(bytes.as_slice()).map_err(|_| ArchiveError::BadProtobuf)?;
        Ok(Some(Frame::from_proto(frame)))
    }

    /// The stream ended between two frames: verify the MAC of what was decrypted (the file
    /// could have changed since the first pass).
    fn finish(&mut self) -> Result<Option<Frame>, ArchiveError> {
        let State::Open(input) = std::mem::replace(&mut self.state, State::Done) else {
            return Ok(None);
        };
        self.decompressed = input.get_ref().read;
        let frames = (*input).into_inner().inner;
        block_on(frames.verify_hmac()).map_err(|error| match error {
            VerifyHmacError::HmacMismatch(_) => ArchiveError::Hmac,
            VerifyHmacError::Io(error) => classify(error),
        })?;
        Ok(None)
    }
}

impl fmt::Debug for ArchiveReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveReader")
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

/// A frame's length prefix, or `None` at the clean end of the stream (no byte at all).
async fn read_length<R: AsyncRead + Unpin>(input: &mut R) -> Result<Option<u32>, ArchiveError> {
    let mut value: u64 = 0;
    for index in 0..MAX_VARINT_BYTES {
        let mut byte = [0u8; 1];
        if input.read(&mut byte).await.map_err(classify)? == 0 {
            // The stream ends: cleanly between frames, or inside a prefix.
            return if index == 0 {
                Ok(None)
            } else {
                Err(ArchiveError::Truncated)
            };
        }
        value |= u64::from(byte[0] & 0x7f) << (7 * index);
        if byte[0] & 0x80 == 0 {
            return u32::try_from(value)
                .map(Some)
                .map_err(|_| ArchiveError::BadFraming);
        }
    }
    Err(ArchiveError::BadFraming)
}

/// The length as a `usize`, or [`ArchiveError::FrameTooLong`] when it is over the limit.
fn check_length(len: u32, limits: &Limits) -> Result<usize, ArchiveError> {
    if len > limits.max_frame_bytes {
        return Err(ArchiveError::FrameTooLong {
            len: u64::from(len),
            max: u64::from(limits.max_frame_bytes),
        });
    }
    usize::try_from(len).map_err(|_| ArchiveError::FrameTooLong {
        len: u64::from(len),
        max: u64::from(limits.max_frame_bytes),
    })
}

/// The `len` bytes of a frame (already within the limit). Memory is taken in steps as the
/// bytes really arrive, so a prefix that lies costs one step, and the buffer ends at exactly
/// `len` (not twice that, as reading "to the end" would cost). Every buffer that held a part
/// of the frame is wiped when it goes, also the smaller ones a growing step replaces.
async fn read_payload<R: AsyncRead + Unpin>(
    input: &mut R,
    len: usize,
) -> Result<Zeroizing<Vec<u8>>, ArchiveError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(len.min(FIRST_ALLOCATION)));
    while bytes.len() < len {
        let filled = bytes.len();
        let step = (len - filled).min(FIRST_ALLOCATION);
        if bytes.capacity() < filled + step {
            // (Not `Vec`'s own growth: it frees the old buffer as it is.)
            let room = (bytes.capacity() * 2).clamp(filled + step, len);
            let mut bigger = Zeroizing::new(Vec::with_capacity(room));
            bigger.extend_from_slice(&bytes);
            bytes = bigger;
        }
        bytes.resize(filled + step, 0);
        input
            .read_exact(&mut bytes[filled..])
            .await
            .map_err(classify)?;
    }
    Ok(bytes)
}

/// How deep messages may nest inside a frame (prost's own limit for decoding).
const MAX_DEPTH: u32 = 100;

/// Refuses a frame that would decode to far more than its bytes: an empty element of a
/// repeated message is two bytes in the file and a whole struct in memory (a megabyte of
/// them decoded to 436 MB), so the bytes of a frame do not bound what it costs. This counts
/// the fields of the frame without a schema, every length-delimited value that parses as a
/// message included (which is at least what the decoder will take for one), and stops at
/// [`Limits::max_frame_fields`]. A value that does not parse as a message is one field.
fn check_fields(bytes: &[u8], limits: &Limits) -> Result<(), ArchiveError> {
    let mut left = limits.max_frame_fields;
    match count_fields(bytes, 0, &mut left) {
        Fields::Counted => Ok(()),
        // (The decoder says what is wrong with it, as before.)
        Fields::NotAMessage => Ok(()),
        Fields::TooMany => Err(ArchiveError::FrameTooComplex {
            max: limits.max_frame_fields,
        }),
    }
}

enum Fields {
    Counted,
    NotAMessage,
    TooMany,
}

/// Walks the wire format of `buf` as a message, taking one from `left` per field.
fn count_fields(mut buf: &[u8], depth: u32, left: &mut u64) -> Fields {
    fn varint(buf: &mut &[u8]) -> Option<u64> {
        let mut value = 0u64;
        for index in 0..10 {
            let (&byte, rest) = buf.split_first()?;
            *buf = rest;
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    while !buf.is_empty() {
        let Some(key) = varint(&mut buf) else {
            return Fields::NotAMessage;
        };
        if key >> 3 == 0 {
            return Fields::NotAMessage;
        }
        if *left == 0 {
            return Fields::TooMany;
        }
        *left -= 1;
        let skip = match key & 7 {
            0 => {
                if varint(&mut buf).is_none() {
                    return Fields::NotAMessage;
                }
                0
            }
            1 => 8,
            5 => 4,
            2 => {
                let Some(len) = varint(&mut buf).and_then(|l| usize::try_from(l).ok()) else {
                    return Fields::NotAMessage;
                };
                if len > buf.len() {
                    return Fields::NotAMessage;
                }
                let (value, rest) = buf.split_at(len);
                buf = rest;
                if depth + 1 >= MAX_DEPTH {
                    // The decoder refuses what nests deeper; so does this.
                    return Fields::TooMany;
                }
                let before = *left;
                match count_fields(value, depth + 1, left) {
                    Fields::Counted => {}
                    // A string or bytes: the one field already counted.
                    Fields::NotAMessage => *left = before,
                    Fields::TooMany => return Fields::TooMany,
                }
                0
            }
            // Groups are not in the format.
            _ => return Fields::NotAMessage,
        };
        if skip > buf.len() {
            return Fields::NotAMessage;
        }
        buf = &buf[skip..];
    }
    Fields::Counted
}

/// What libsignal's `FramesReader::new` can fail with.
fn from_validation(error: ValidationError) -> ArchiveError {
    match error {
        ValidationError::Io(error) => classify(error),
        ValidationError::InvalidHmac(_) => ArchiveError::Hmac,
        ValidationError::MissingMetadataField(_)
        | ValidationError::InvalidLength { .. }
        | ValidationError::TooManyForwardSecrecyPairs(_) => ArchiveError::BadHeader,
    }
}

/// Maps an I/O error of the decrypting stream to what is wrong with the file.
///
/// libsignal's AES reader reports bad PKCS7 padding as a bare `InvalidData` (no payload), the
/// gzip decoder reports bad data as `InvalidData` or `InvalidInput` with a payload, and a stream
/// that ends early as `UnexpectedEof`. The tests pin each of these against the pinned libsignal,
/// so a bump that changes one fails them.
fn classify(error: io::Error) -> ArchiveError {
    if let Some(cap) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<CapExceeded>())
    {
        return ArchiveError::TooLarge { max: cap.max };
    }
    if error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<HeaderFormat>().is_some())
    {
        return ArchiveError::BadHeader;
    }
    if error.raw_os_error().is_some() {
        return ArchiveError::Io(error.kind());
    }
    match error.kind() {
        io::ErrorKind::UnexpectedEof => ArchiveError::Truncated,
        io::ErrorKind::InvalidData if error.get_ref().is_none() => ArchiveError::BadPadding,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => ArchiveError::BadGzip,
        kind => ArchiveError::Io(kind),
    }
}

/// The error [`Capped`] raises, recognised by [`classify`].
#[derive(Debug)]
struct CapExceeded {
    max: u64,
}

impl fmt::Display for CapExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "more than {} decompressed bytes", self.max)
    }
}

impl std::error::Error for CapExceeded {}

/// Lets `max` bytes through and fails on the next one, so a gzip bomb stops at the cap and an
/// archive of exactly `max` bytes still ends cleanly.
struct Capped<R> {
    inner: R,
    read: u64,
    max: u64,
}

impl<R: AsyncRead + Unpin> AsyncRead for Capped<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        // One byte more than the room left: enough to tell "the stream ends here" (0 bytes)
        // from "there is more than the cap".
        let room = this.max.saturating_sub(this.read).saturating_add(1);
        let want = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        match Pin::new(&mut this.inner).poll_read(cx, &mut buf[..want]) {
            Poll::Ready(Ok(n)) => {
                this.read += n as u64;
                if this.read > this.max {
                    Poll::Ready(Err(io::Error::other(CapExceeded { max: this.max })))
                } else {
                    Poll::Ready(Ok(n))
                }
            }
            other => other,
        }
    }
}
