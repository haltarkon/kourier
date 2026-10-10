//! The auto-download policy (§5.8): images, stickers, voice notes and files up to 25 MiB by
//! default, videos and bigger files on click, never above the network's receive limit.

use kourier_model::{AttachmentKind, AttachmentState, AutoDownloadPolicy};

/// Largest image, sticker, voice note or video fetched without a click (files follow
/// [`AutoDownloadPolicy::files_max_bytes`]).
pub(crate) const AUTO_MEDIA_MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Largest long text (`Attachment.long_text`) the core fetches on its own and joins into the
/// message's body (Signal allows 64 KiB); a larger one stays a file to click.
pub(crate) const MAX_LONG_TEXT_BYTES: u64 = 256 * 1024;

/// What to do with a new attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Download now.
    Auto,
    /// Wait for the user.
    OnClick,
    /// Larger than the network lets us receive: never downloaded.
    TooLarge,
}

/// The facts the policy looks at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Candidate {
    /// Attachment kind.
    pub kind: AttachmentKind,
    /// Plaintext size from the pointer (0 = unknown).
    pub size: u64,
    /// The message is a sticker.
    pub sticker: bool,
    /// The whole text of a message whose body arrived trimmed (`Attachment.long_text`).
    pub long_text: bool,
    /// Current state.
    pub state: AttachmentState,
    /// A network pointer exists.
    pub has_remote: bool,
    /// Imported history (bulk, never fetched on its own).
    pub history: bool,
    /// A blocked conversation or a pending message request: nothing is fetched from strangers
    /// before the user accepts (Signal Desktop does the same).
    pub untrusted: bool,
}

/// Whether `c` is downloaded automatically. `receive_limit` is the network's limit
/// (`AccountCaps::max_receive_attachment_bytes`).
pub(crate) fn decide(
    c: &Candidate,
    prefs: &AutoDownloadPolicy,
    receive_limit: Option<u64>,
) -> Decision {
    if !c.has_remote || c.state != AttachmentState::Remote {
        return Decision::OnClick;
    }
    if receive_limit.is_some_and(|limit| c.size > limit) {
        return Decision::TooLarge;
    }
    if c.history || c.untrusted {
        return Decision::OnClick;
    }
    let media = |enabled: bool| enabled && c.size <= AUTO_MEDIA_MAX_BYTES;
    let auto = if c.long_text {
        // The rest of the message's text, not a file: fetched whatever the settings say, as
        // long as it has the size of a text.
        c.size <= MAX_LONG_TEXT_BYTES
    } else if c.sticker {
        media(prefs.stickers)
    } else {
        match c.kind {
            AttachmentKind::Image | AttachmentKind::Gif => media(prefs.images),
            AttachmentKind::Voice => media(prefs.voice),
            AttachmentKind::Video => media(prefs.videos),
            AttachmentKind::Audio | AttachmentKind::File => c.size <= prefs.files_max_bytes,
        }
    };
    if auto {
        Decision::Auto
    } else {
        Decision::OnClick
    }
}

/// Whether the user may (re)start a download of an attachment in `state` of `size` bytes.
pub(crate) fn may_click(
    state: AttachmentState,
    has_remote: bool,
    size: u64,
    receive_limit: Option<u64>,
) -> Result<(), &'static str> {
    if !has_remote {
        return Err("the attachment has no network pointer");
    }
    if receive_limit.is_some_and(|limit| size > limit) {
        return Err("the attachment is larger than the network allows");
    }
    match state {
        AttachmentState::Remote | AttachmentState::Failed { .. } => Ok(()),
        AttachmentState::Downloading { .. } | AttachmentState::Local => {
            Err("the attachment is downloading or downloaded")
        }
        AttachmentState::Expired => Err("the network no longer has the attachment"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn c(kind: AttachmentKind, size: u64) -> Candidate {
        Candidate {
            kind,
            size,
            sticker: false,
            long_text: false,
            state: AttachmentState::Remote,
            has_remote: true,
            history: false,
            untrusted: false,
        }
    }

    fn d(c: &Candidate) -> Decision {
        decide(c, &AutoDownloadPolicy::default(), None)
    }

    #[test]
    fn defaults_fetch_images_stickers_voice_and_small_files() {
        use AttachmentKind::*;
        for kind in [Image, Gif, Voice] {
            assert_eq!(d(&c(kind, 3 * MIB)), Decision::Auto, "{kind:?}");
        }
        assert_eq!(d(&c(File, 25 * MIB)), Decision::Auto);
        assert_eq!(d(&c(Audio, MIB)), Decision::Auto);
        assert_eq!(d(&c(File, 25 * MIB + 1)), Decision::OnClick);
        assert_eq!(d(&c(Image, 25 * MIB + 1)), Decision::OnClick);
        assert_eq!(d(&c(Video, MIB)), Decision::OnClick, "videos on click");
        let sticker = Candidate {
            sticker: true,
            ..c(Image, 50_000)
        };
        assert_eq!(d(&sticker), Decision::Auto);
        // Unknown size (0): the backend enforces the limit while streaming.
        assert_eq!(d(&c(Image, 0)), Decision::Auto);
    }

    #[test]
    fn user_preferences_apply() {
        use AttachmentKind::*;
        let prefs = AutoDownloadPolicy {
            images: false,
            stickers: false,
            voice: false,
            videos: true,
            files_max_bytes: 0,
        };
        let sticker = Candidate {
            sticker: true,
            ..c(Image, 10)
        };
        assert_eq!(decide(&c(Image, 10), &prefs, None), Decision::OnClick);
        assert_eq!(decide(&sticker, &prefs, None), Decision::OnClick);
        assert_eq!(decide(&c(Voice, 10), &prefs, None), Decision::OnClick);
        assert_eq!(decide(&c(File, 10), &prefs, None), Decision::OnClick);
        assert_eq!(decide(&c(Video, 10), &prefs, None), Decision::Auto);
        // The rest of a text is fetched even with file downloads off, unless it is no text
        // by its size or comes from a stranger.
        let text = Candidate {
            long_text: true,
            ..c(File, 60_000)
        };
        assert_eq!(decide(&text, &prefs, None), Decision::Auto);
        let huge = Candidate {
            size: MAX_LONG_TEXT_BYTES + 1,
            ..text
        };
        assert_eq!(decide(&huge, &prefs, None), Decision::OnClick);
        let stranger = Candidate {
            untrusted: true,
            ..text
        };
        assert_eq!(decide(&stranger, &prefs, None), Decision::OnClick);
    }

    #[test]
    fn never_above_the_receive_limit() {
        let limit = Some(10 * MIB);
        let prefs = AutoDownloadPolicy {
            files_max_bytes: u64::MAX,
            ..AutoDownloadPolicy::default()
        };
        let big = c(AttachmentKind::File, 10 * MIB + 1);
        assert_eq!(decide(&big, &prefs, limit), Decision::TooLarge);
        assert_eq!(
            decide(&c(AttachmentKind::File, 10 * MIB), &prefs, limit),
            Decision::Auto
        );
        // History and untrusted chats still learn that it is too large.
        let old = Candidate {
            history: true,
            ..big
        };
        assert_eq!(decide(&old, &prefs, limit), Decision::TooLarge);
        assert!(may_click(AttachmentState::Remote, true, 10 * MIB + 1, limit).is_err());
        assert!(may_click(AttachmentState::Remote, true, 10 * MIB, limit).is_ok());
    }

    #[test]
    fn history_untrusted_and_non_remote_wait_for_a_click() {
        let img = c(AttachmentKind::Image, 1000);
        for cand in [
            Candidate {
                history: true,
                ..img
            },
            Candidate {
                untrusted: true,
                ..img
            },
            Candidate {
                has_remote: false,
                ..img
            },
            Candidate {
                state: AttachmentState::Local,
                ..img
            },
            Candidate {
                state: AttachmentState::Failed { retryable: true },
                ..img
            },
        ] {
            assert_eq!(d(&cand), Decision::OnClick, "{cand:?}");
        }
    }

    #[test]
    fn clicks_restart_remote_and_failed_only() {
        assert!(may_click(AttachmentState::Remote, true, 1, None).is_ok());
        assert!(may_click(AttachmentState::Failed { retryable: false }, true, 1, None).is_ok());
        for s in [
            AttachmentState::Local,
            AttachmentState::Expired,
            AttachmentState::Downloading { done: 0, total: 1 },
        ] {
            assert!(may_click(s, true, 1, None).is_err(), "{s:?}");
        }
        assert!(may_click(AttachmentState::Remote, false, 1, None).is_err());
    }
}
