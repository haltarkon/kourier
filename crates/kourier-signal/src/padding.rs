//! Content padding: the serialized `Content` is followed by `0x80` and zeros up to one byte short
//! of a multiple of 80, so ciphertext lengths only leak the 80-byte block count (§6.7).
//!
//! Port of `padMessage` (`SD/ts/textsecure/OutgoingMessage.preload.ts:123-148`) and `#unpad`
//! (`SD/ts/textsecure/MessageReceiver.preload.ts:1691-1702`).

/// Padding block size in bytes.
const PADDING_BLOCK: usize = 80;

/// The marker byte that ends the payload.
const TERMINATOR: u8 = 0x80;

/// The padding is not `0x80` followed by zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid content padding")]
pub(crate) struct PaddingError;

/// Desktop's `getPaddedMessageLength`: `len + 1` (terminator) rounded up to the next multiple
/// of 80.
fn padded_message_length(len: usize) -> usize {
    (len + 1).div_ceil(PADDING_BLOCK) * PADDING_BLOCK
}

/// Pads a serialized `Content` for encryption: `message ‖ 0x80 ‖ 0x00…`, total length
/// `padded_message_length(len + 1) - 1` exactly as Desktop computes it (79, 159, 239, … bytes).
pub(crate) fn pad(message: &[u8]) -> Vec<u8> {
    let total = padded_message_length(message.len() + 1) - 1;
    let mut padded = Vec::with_capacity(total);
    padded.extend_from_slice(message);
    padded.push(TERMINATOR);
    padded.resize(total, 0);
    padded
}

/// Strips the padding of a decrypted `Content`: everything before the last `0x80` that is
/// followed only by zeros. Like Desktop, input without any `0x80` is returned unchanged when it
/// is all zeros (or empty); any other trailing byte is an error.
pub(crate) fn unpad(padded: &[u8]) -> Result<&[u8], PaddingError> {
    for (i, byte) in padded.iter().enumerate().rev() {
        match *byte {
            TERMINATOR => return Ok(&padded[..i]),
            0 => {}
            _ => return Err(PaddingError),
        }
    }
    Ok(padded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_lengths_match_desktop() {
        // Desktop: getPaddedMessageLength(len + 1) - 1.
        for (len, expected) in [(0, 79), (1, 79), (77, 79), (78, 79), (79, 159), (158, 159)] {
            assert_eq!(pad(&vec![7; len]).len(), expected, "len {len}");
        }
        assert_eq!(pad(&[7; 159]).len(), 239);
    }

    #[test]
    fn pad_layout_is_payload_marker_zeros() {
        let padded = pad(b"hello");
        assert_eq!(&padded[..5], b"hello");
        assert_eq!(padded[5], 0x80);
        assert!(padded[6..].iter().all(|b| *b == 0));
    }

    #[test]
    fn round_trips_every_length_up_to_three_blocks() {
        for len in 0..=240 {
            let message: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            assert_eq!(unpad(&pad(&message)), Ok(message.as_slice()), "len {len}");
        }
    }

    #[test]
    fn payload_ending_in_marker_or_zero_survives() {
        for message in [&[1, 0x80][..], &[0x80, 0, 0], &[0, 0]] {
            assert_eq!(unpad(&pad(message)), Ok(message));
        }
    }

    #[test]
    fn unpad_rejects_garbage_after_payload() {
        assert_eq!(unpad(&[1, 2, 3]), Err(PaddingError));
        assert_eq!(unpad(&[1, 0x80, 0, 5, 0]), Err(PaddingError));
    }

    #[test]
    fn unpad_without_marker_keeps_zero_input_like_desktop() {
        assert_eq!(unpad(&[]), Ok(&[][..]));
        assert_eq!(unpad(&[0, 0, 0]), Ok(&[0, 0, 0][..]));
    }
}
