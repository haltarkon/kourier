//! Streaming, authenticating blob reader.

use crate::format::{HEADER_LEN, Sealer, TAG_LEN};
use crate::{BlobError, FileKey};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use zeroize::Zeroizing;

/// Decrypts a blob chunk by chunk. Each chunk is authenticated before any of its bytes are
/// returned; a missing last chunk surfaces as [`BlobError::Truncated`] at the end, so a
/// consumer trusts the data only after `read` returned `Ok(0)`. Errors come through `std::io`;
/// recover them with [`BlobError::from_io`].
pub struct BlobReader<R> {
    inner: R,
    sealer: Sealer,
    /// Raw ciphertext read ahead: one chunk plus one byte tells "last chunk" apart.
    raw: Vec<u8>,
    eof: bool,
    plain: Zeroizing<Vec<u8>>,
    pos: usize,
    index: u32,
    done: bool,
}

impl BlobReader<File> {
    /// Opens the blob at `path`.
    pub fn open(path: &Path, key: &FileKey) -> Result<Self, BlobError> {
        Self::new(File::open(path)?, key)
    }
}

impl<R: Read> BlobReader<R> {
    /// Reads and checks the header from `inner`.
    pub fn new(mut inner: R, key: &FileKey) -> Result<Self, BlobError> {
        let mut header = [0u8; HEADER_LEN];
        let got = read_full(&mut inner, &mut header)?;
        if got < HEADER_LEN {
            return Err(if got >= 8 && header[..8] == crate::MAGIC {
                BlobError::Truncated
            } else {
                BlobError::NotABlob
            });
        }
        let sealer = Sealer::parse(key, header)?;
        let cap = sealer.chunk_size + TAG_LEN + 1;
        Ok(Self {
            inner,
            sealer,
            raw: Vec::with_capacity(cap),
            eof: false,
            plain: Zeroizing::new(Vec::new()),
            pos: 0,
            index: 0,
            done: false,
        })
    }

    fn next_chunk(&mut self) -> Result<(), BlobError> {
        let ct_chunk = self.sealer.chunk_size + TAG_LEN;
        while !self.eof && self.raw.len() <= ct_chunk {
            let start = self.raw.len();
            self.raw.resize(ct_chunk + 1, 0);
            let n = loop {
                match self.inner.read(&mut self.raw[start..]) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    other => break other,
                }
            };
            let n = match n {
                Ok(n) => n,
                Err(e) => {
                    self.raw.truncate(start);
                    return Err(e.into());
                }
            };
            self.raw.truncate(start + n);
            self.eof = n == 0;
        }
        let last = self.raw.len() <= ct_chunk;
        if last && self.raw.is_empty() {
            // The header, or a chunk flagged "not last", is followed by nothing.
            return Err(BlobError::Truncated);
        }
        let mut chunk = Zeroizing::new(if last {
            std::mem::take(&mut self.raw)
        } else {
            let rest = self.raw.split_off(ct_chunk);
            std::mem::replace(&mut self.raw, rest)
        });
        if chunk.len() < TAG_LEN {
            return Err(BlobError::Corrupt);
        }
        if let Err(e) = self.sealer.open(self.index, last, &mut chunk) {
            // A full chunk that only authenticates as "not last": the file was cut after it.
            if last && chunk.len() == ct_chunk {
                let mut probe = Zeroizing::new(chunk.to_vec());
                if self.sealer.open(self.index, false, &mut probe).is_ok() {
                    return Err(BlobError::Truncated);
                }
            }
            return Err(e);
        }
        if !last {
            self.index = self.index.checked_add(1).ok_or(BlobError::TooLarge)?;
        }
        self.plain = chunk;
        self.pos = 0;
        self.done = last;
        Ok(())
    }
}

impl<R: Read> Read for BlobReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.plain.len() {
            if self.done || out.is_empty() {
                return Ok(0);
            }
            self.next_chunk().map_err(BlobError::into_io)?;
        }
        let n = out.len().min(self.plain.len() - self.pos);
        out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}
