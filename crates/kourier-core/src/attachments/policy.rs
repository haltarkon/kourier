//! The auto-download policy (§5.8): images, stickers, voice notes and files up to 25 MiB by
//! default, videos and bigger files on click, never above the network's receive limit.

use kourier_model::{AskFailure, AttachmentKind, AttachmentState, AutoDownloadPolicy};

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
    // Imported history waits for a click, all but the rest of a long text (user decision,
    // 2026-10-10: "fetch automatically"): a backend gives an imported long text a pointer only
    // while the server may still have it, and without it the message stays cut off.
    if (c.history && !c.long_text) || c.untrusted {
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

/// What a click on an attachment does ([`may_click`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Click {
    /// Download it from its pointer.
    Download,
    /// The network no longer has it: ask the account's other device to upload it again
    /// (Signal's attachment backfill). Also a click on an attachment being asked for already
    /// (it joins the open request).
    Ask,
}

/// What a click on an attachment in `state` of `size` bytes does, or why it does nothing.
/// `can_ask` says whether the account's other device can be asked for this attachment (the
/// account is linked, its network can ask, the message can be named, it is not view-once,
/// it is a file and not the rest of a long text).
///
/// The size comes first: neither a download nor the other device can bring a file above the
/// network's receive limit. The gone states come before the pointer: an `Unavailable`
/// attachment has none, an `Expired` one a dead one.
pub(crate) fn may_click(
    state: AttachmentState,
    has_remote: bool,
    size: u64,
    receive_limit: Option<u64>,
    can_ask: bool,
) -> Result<Click, &'static str> {
    if receive_limit.is_some_and(|limit| size > limit) {
        return Err("the attachment is larger than the network allows");
    }
    let ask = || {
        if can_ask {
            Ok(Click::Ask)
        } else {
            Err("the attachment is not available on this device")
        }
    };
    match state {
        AttachmentState::Remote | AttachmentState::Failed { .. } if has_remote => {
            Ok(Click::Download)
        }
        AttachmentState::Remote | AttachmentState::Failed { .. } => {
            Err("the attachment has no network pointer")
        }
        AttachmentState::Downloading { .. } | AttachmentState::Local => {
            Err("the attachment is downloading or downloaded")
        }
        AttachmentState::Expired
        | AttachmentState::Unavailable
        | AttachmentState::Asking
        | AttachmentState::AskFailed {
            why: AskFailure::NoAnswer,
        } => ask(),
        AttachmentState::AskFailed {
            why: AskFailure::NoMessage,
        } => Err("the account's other device no longer has the message"),
        AttachmentState::AskFailed {
            why: AskFailure::NoFile,
        } => Err("the account's other device does not have the attachment"),
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
        assert!(may_click(AttachmentState::Remote, true, 10 * MIB + 1, limit, true).is_err());
        assert!(may_click(AttachmentState::Remote, true, 10 * MIB, limit, true).is_ok());
        // The other device cannot bring a file above the limit either.
        assert!(
            may_click(
                AttachmentState::Unavailable,
                false,
                10 * MIB + 1,
                limit,
                true
            )
            .is_err()
        );
        assert!(may_click(AttachmentState::Expired, true, 10 * MIB + 1, limit, true).is_err());
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
    fn clicks_download_remote_and_failed() {
        let click = |s, remote| may_click(s, remote, 1, None, true);
        assert_eq!(click(AttachmentState::Remote, true), Ok(Click::Download));
        let failed = AttachmentState::Failed { retryable: false };
        assert_eq!(click(failed, true), Ok(Click::Download));
        for s in [
            AttachmentState::Local,
            AttachmentState::Downloading { done: 0, total: 1 },
        ] {
            assert!(click(s, true).is_err(), "{s:?}");
        }
        assert!(click(AttachmentState::Remote, false).is_err());
        assert!(click(failed, false).is_err());
    }

    /// S6: a file the network lost (`Unavailable`: no pointer, `Expired`: a dead one) asks
    /// the other device, also after it did not answer; never after it said it has neither the
    /// message nor the file, and never where it cannot be asked.
    #[test]
    fn clicks_on_gone_files_ask_the_other_device() {
        let no_answer = AttachmentState::AskFailed {
            why: AskFailure::NoAnswer,
        };
        for (s, remote) in [
            (AttachmentState::Unavailable, false),
            (AttachmentState::Expired, true),
            (AttachmentState::Expired, false),
            (AttachmentState::Asking, false),
            (no_answer, false),
            (no_answer, true),
        ] {
            assert_eq!(may_click(s, remote, 1, None, true), Ok(Click::Ask), "{s:?}");
            assert!(may_click(s, remote, 1, None, false).is_err(), "{s:?}");
        }
        for why in [AskFailure::NoMessage, AskFailure::NoFile] {
            let s = AttachmentState::AskFailed { why };
            assert!(may_click(s, false, 1, None, true).is_err(), "{s:?}");
            assert!(may_click(s, true, 1, None, true).is_err(), "{s:?}");
        }
    }

    /// The one exception to "history waits for a click": the rest of an imported long text
    /// (user decision, 2026-10-10), while it has a pointer and the size of a text; never in a
    /// chat that is a request or blocked, and never anything else of the history.
    #[test]
    fn the_rest_of_an_imported_long_text_is_fetched_by_itself() {
        let text = Candidate {
            long_text: true,
            history: true,
            ..c(AttachmentKind::File, 3000)
        };
        assert_eq!(d(&text), Decision::Auto);
        for cand in [
            Candidate {
                untrusted: true,
                ..text
            },
            Candidate {
                has_remote: false,
                state: AttachmentState::Unavailable,
                ..text
            },
            Candidate {
                size: MAX_LONG_TEXT_BYTES + 1,
                ..text
            },
            Candidate {
                long_text: false,
                ..text
            },
            Candidate {
                long_text: false,
                sticker: true,
                ..text
            },
        ] {
            assert_eq!(d(&cand), Decision::OnClick, "{cand:?}");
        }
    }
}
