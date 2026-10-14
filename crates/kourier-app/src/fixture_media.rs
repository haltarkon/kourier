//! Media of the fixture data set (`--fixture`, §10.2): a file and a voice note in the
//! conversation with the first photo, and profile pictures for the first contacts, so the
//! screenshots show the attachment blocks and round avatars. The fake network delivers the
//! messages and the contacts' avatar references; the avatar files are written into the
//! account's blob directory (encrypted, like a backend writes them) once the account exists.

use std::path::{Path, PathBuf};

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_blob::FileKey;
use kourier_fake::attachments::{synthetic_file, synthetic_image};
use kourier_fake::{Script, Step};
use kourier_model::{
    Attachment, AttachmentKind, BlobKey, BlobRef, Body, ContactPatch, ConvKey, ConversationPatch,
    MsgTarget, Patch, Placeholder, Text, UserId,
};

/// One more message in the showcase chat with the timer: received, read (so its time runs
/// from its delivery) and gone `timer` later (`FixtureOptions::expiring`). It is the chat's
/// newest message but one second, so the fixture's "now" stays where it was.
pub(crate) fn with_expiring(script: Script, timer: std::time::Duration) -> Script {
    let Some(newest) = newest_message(&script) else {
        return script;
    };
    let Ok(key) = kourier_fake::fixture::showcase_timer() else {
        return script;
    };
    let ConvKey::Direct(peer) = &key else {
        return script;
    };
    let at = newest - 1_000;
    let mut message =
        kourier_fake::scripts::remote(&key, peer, "expiring", at, crate::app::EXPIRING_TEXT);
    message.read = true;
    message.expiry = Some(kourier_model::Expiry {
        kind: kourier_model::DisappearKind::AfterRead,
        timer,
        started_at: None,
    });
    deliver_before_final_state(script, vec![BackendEvent::Message(message)])
}

/// What happens after the fixture data is in (`FixtureOptions::arrivals`): the peer of the
/// showcase chat with the timer writes `arrivals.count` messages, one every
/// `arrivals.every`. Each is read where it arrives, so one with a lifetime disappears that
/// long after its delivery. `now` is the fixture's "now" (ms); the messages are dated after
/// it.
pub(crate) fn with_arrivals(
    mut script: Script,
    arrivals: crate::app::Arrivals,
    now: i64,
) -> Script {
    let Ok(key) = kourier_fake::fixture::showcase_timer() else {
        return script;
    };
    let ConvKey::Direct(peer) = &key else {
        return script;
    };
    let message = |local: String, at: i64, text: String| {
        let mut message = kourier_fake::scripts::remote(&key, peer, &local, at, &text);
        message.read = true;
        message.expiry = arrivals.lifetime.map(|timer| kourier_model::Expiry {
            kind: kourier_model::DisappearKind::AfterRead,
            timer,
            started_at: None,
        });
        BackendEvent::Message(message)
    };
    let every = i64::try_from(arrivals.every.as_millis()).unwrap_or(i64::MAX);
    for n in 1..=arrivals.count {
        let at = now.saturating_add(every.saturating_mul(i64::from(n)));
        script = script
            .step(Step::Sleep(arrivals.every))
            .deliver(vec![message(
                format!("arrival-{n}"),
                at,
                format!("Arrival {n}"),
            )]);
    }
    script
}

/// Contacts that get a profile picture.
const AVATARS: usize = 6;

/// Side of a fixture profile picture.
const AVATAR_SIDE: u32 = 96;

/// Avatar files to write once the fixture account is linked.
#[derive(Default)]
pub(crate) struct FixtureAvatars {
    files: Vec<(PathBuf, FileKey, Vec<u8>)>,
}

impl FixtureAvatars {
    /// Writes the files below the account's blob directory.
    pub(crate) fn write(&self, blob_dir: &Path) -> std::io::Result<()> {
        for (rel, key, png) in &self.files {
            let path = blob_dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            kourier_blob::write_all(&path, key, png).map_err(std::io::Error::other)?;
        }
        Ok(())
    }
}

/// Appends a delivery to `script`, before the step that puts the account into its final
/// state (a connection state or the unlink: nothing can be delivered after that one).
fn deliver_before_final_state(mut script: Script, events: Vec<BackendEvent>) -> Script {
    let last = match script.steps.last() {
        Some(Step::Conn(_) | Step::Unlink { .. }) => script.steps.pop(),
        _ => None,
    };
    script = script.deliver(events);
    script.steps.extend(last);
    script
}

/// The newest message of the script (ms): fixture mode's "now" is a little after it.
pub(crate) fn newest_message(script: &Script) -> Option<i64> {
    script
        .deliveries()
        .iter()
        .flatten()
        .filter_map(|e| match e {
            BackendEvent::Message(m) => Some(m.sent_at),
            _ => None,
        })
        .max()
}

/// The contact of the chat with stand-ins for what Kourier does not show yet
/// ([`with_showcase_extras`]).
pub(crate) fn kinds_peer() -> Option<UserId> {
    UserId::parse("aci:00000000-0000-4000-8000-000000f12003").ok()
}

/// The contact of the chat with albums and a message edited twice
/// ([`with_showcase_extras`]).
pub(crate) fn albums_peer() -> Option<UserId> {
    UserId::parse("aci:00000000-0000-4000-8000-000000f12004").ok()
}

/// The contact of the chat with one long message ([`with_showcase_extras`]).
pub(crate) fn readmore_peer() -> Option<UserId> {
    UserId::parse("aci:00000000-0000-4000-8000-000000f12005").ok()
}

/// The long message of that chat (`--page=readmore`): four paragraphs, the last with a link,
/// more than the timeline shows at first and less than one "Read more" adds.
pub(crate) const LONG_TEXT: &str = concat!(
    "Notes from the weekend at the ridge, in case you want to repeat it. We left the car at the ",
    "old quarry around seven, when the fog still sat in the valley, and walked up the western ",
    "path with both big packs and the repair kit. The first hour is the steepest and there is ",
    "no shade, so start early and carry more water than you think you need.\n\n",
    "Around nine the path levels out and you reach the first meadow. That is where we launched ",
    "the long-range wing: the wind was steady from the south at about four metres a second, and ",
    "the climb-out was clean. The battery lost eleven percent on the way up to the ridge line, ",
    "which is a lot, so I would plan the return leg with a wide margin. The video link held ",
    "without a single drop until we crossed the second saddle, and then the picture broke up for ",
    "a few seconds each time the wing went behind the rock face.\n\n",
    "Lunch was at the hut. The keeper let us charge the packs from his solar panel in exchange ",
    "for two photos of the roof, which he wants to show the insurance people. Afterwards we flew ",
    "the loop over the lake twice, once low and once high, and compared the logs. The low one is ",
    "smoother in the video, but the high one gives a much better view of the old mine workings ",
    "on the far shore.\n\n",
    "On the way down we found the trail marker that was missing last year, somebody put a new ",
    "post in with a yellow cap. The coordinates are in the shared map: ",
    "https://example.org/ridge/notes-2026 and the track file is attached to the same page. Next ",
    "time we should bring a second wing, a proper windsock and some real food instead of only ",
    "chocolate. Let me know which weekend works for you and I will book the hut.",
);

/// The fixture account's own number (555-01xx numbers are reserved for fiction).
pub(crate) const FIXTURE_NUMBER: &str = "+1 202 555 0143";

/// What the fake's showcase chats do not have: a chat with one message of each kind Kourier
/// shows a stand-in for (view-once media, a payment, a poll, …), a chat with a sticker, albums
/// of two, three, four and more pictures than the grid has tiles and a text edited twice, a
/// chat with one long text, and reactions of several members on the newest own text of the
/// first group (the "who reacted" list).
pub(crate) fn with_showcase_extras(script: Script) -> Script {
    // The account's own number, as a network reports it (one of the numbers reserved for
    // fiction): shown where the account is described.
    let script = if script.number.is_none() {
        script.with_number(FIXTURE_NUMBER)
    } else {
        script
    };
    let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
    let Some(newest) = newest_message(&script) else {
        return script;
    };
    let mut extra = Vec::new();

    // ---- stand-ins ----
    if let Some(peer) = kinds_peer() {
        let key = ConvKey::Direct(peer.clone());
        let mut contact = ContactPatch::new(peer.clone());
        contact.profile_name = Patch::Set("Wren Tallow".into());
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(kourier_fake::scripts::signal_features(false));
        extra.push(BackendEvent::ContactUpsert(contact));
        extra.push(BackendEvent::ConversationUpsert(patch));
        // Between the two showcase chats of the fake, by time.
        let at = |n: i64| newest - 12 * 60_000 + n * 40_000;
        let unsupported = |kind: &str, summary: Option<&str>| Body::Unsupported {
            kind_hint: kind.to_owned(),
            summary: summary.map(str::to_owned),
        };
        let bodies = [
            Body::text("Trying out a few things from my phone."),
            unsupported("view-once", None),
            unsupported("payment", None),
            unsupported("poll", Some("Friday or Saturday?")),
            unsupported("poll-ended", None),
            unsupported("pin", None),
            unsupported("contact", None),
            unsupported("story-reply", None),
            Body::Placeholder(Placeholder::Undecryptable { resendable: false }),
            unsupported("newer-protocol", None),
        ];
        for (n, body) in bodies.into_iter().enumerate() {
            let n = i64::try_from(n).unwrap_or(0);
            let mut m = kourier_fake::scripts::remote(&key, &peer, &at(n).to_string(), at(n), "");
            m.body = body;
            m.read = true;
            extra.push(BackendEvent::Message(m));
        }
    }

    // ---- albums and an edit history ----
    if let Some(peer) = albums_peer() {
        let key = ConvKey::Direct(peer.clone());
        let mut contact = ContactPatch::new(peer.clone());
        contact.profile_name = Patch::Set("Sol Vireo".into());
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(kourier_fake::scripts::signal_features(false));
        extra.push(BackendEvent::ContactUpsert(contact));
        extra.push(BackendEvent::ConversationUpsert(patch));
        // After the stand-ins chat, before the newest fixture message.
        let at = |n: i64| newest - 9 * 60_000 + n * 30_000;
        let local = |n: i64| at(n).to_string();
        // Pictures in different proportions, each with a look of its own.
        let pictures = |count: u64, seed: u64| -> Vec<kourier_model::Attachment> {
            (0..count)
                .map(|i| {
                    let (w, h) = [(600, 400), (400, 500), (500, 500), (640, 360)]
                        [usize::try_from((seed + i) % 4).unwrap_or(0)];
                    synthetic_image(w, h, seed * 16 + i)
                })
                .collect()
        };
        let album = |n: i64, own: bool, count: u64, caption: Option<&str>| {
            let mut m = if own {
                kourier_fake::scripts::own(&key, &local(n), at(n), "")
            } else {
                kourier_fake::scripts::remote(&key, &peer, &local(n), at(n), "")
            };
            m.body = Body::Media {
                attachments: pictures(count, u64::try_from(n).unwrap_or(0) + 3),
                caption: caption.map(Text::plain),
            };
            m.read = true;
            BackendEvent::Message(m)
        };
        // A sticker, oldest of the chat: it stands without a bubble and has no viewer
        // action of its own (the keyboard's Enter opens its row's menu).
        let mut sticker = kourier_fake::scripts::remote(&key, &peer, &local(-1), at(-1), "");
        sticker.body = Body::Sticker(Box::new(synthetic_image(256, 256, 41)));
        sticker.read = true;
        extra.push(BackendEvent::Message(sticker));
        let mut plan =
            kourier_fake::scripts::remote(&key, &peer, &local(0), at(0), "Meet at the hangar at 5");
        plan.read = true;
        let target = MsgTarget::to_id(Some(key.clone()), &plan.net_id());
        extra.push(BackendEvent::Message(plan));
        for (n, text) in [
            (1, "Meet at the hangar at 6"),
            (2, "Meet at the hangar at 6, and bring the spare props"),
        ] {
            extra.push(BackendEvent::Edited {
                target: target.clone(),
                by: Some(peer.clone()),
                new_local: Some(local(n).into()),
                body: Body::Text(Text::plain(text)),
                edited_at: at(n),
                keeps_quote: None,
            });
        }
        // (Newest last: the bottom of the chat shows a grid of two, of three and of many.)
        extra.push(album(3, false, 4, None));
        // (A caption wider than two pictures side by side: the pair fills the bubble.)
        extra.push(album(
            4,
            false,
            2,
            Some(
                "Before and after the repair: the left boom was cracked right at the motor mount, the new one is carbon.",
            ),
        ));
        extra.push(album(5, true, 3, None));
        extra.push(album(6, false, 9, Some("All nine from the ridge")));
    }

    // ---- one long message ----
    if let Some(peer) = readmore_peer() {
        let key = ConvKey::Direct(peer.clone());
        let mut contact = ContactPatch::new(peer.clone());
        contact.profile_name = Patch::Set("Mika Brandt".into());
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(kourier_fake::scripts::signal_features(false));
        extra.push(BackendEvent::ContactUpsert(contact));
        extra.push(BackendEvent::ConversationUpsert(patch));
        // Before the other showcase chats, by time.
        let at = newest - 15 * 60_000;
        let mut long = kourier_fake::scripts::remote(&key, &peer, &at.to_string(), at, LONG_TEXT);
        long.read = true;
        extra.push(BackendEvent::Message(long));
    }

    // ---- reactions of several members ----
    // In the group with the newest message: the one `--page=chat` opens.
    let newest_in = |key: &ConvKey| {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if &m.conv == key => Some(m.sent_at),
                _ => None,
            })
            .max()
    };
    let group = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) if matches!(p.key, ConvKey::Group(_)) => {
                let members = p.members.as_ref()?;
                let users: Vec<UserId> = members
                    .upsert
                    .iter()
                    .map(|m| m.user.clone())
                    .filter(|u| !script.own_ids.contains(u))
                    .collect();
                Some((p.key.clone(), users))
            }
            _ => None,
        })
        .max_by_key(|(key, _)| newest_in(key));
    if let Some((key, members)) = group {
        let own_text = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m)
                    if m.conv == key && m.origin.is_own() && matches!(m.body, Body::Text(_)) =>
                {
                    Some(m)
                }
                _ => None,
            })
            .max_by_key(|m| m.sent_at);
        if let Some(m) = own_text {
            let target = MsgTarget::to_id(Some(key.clone()), &m.net_id());
            // The first member reacts in the fake's fixture already.
            for (n, (by, emoji)) in members.iter().skip(1).zip(["👍", "👍", "😂"]).enumerate()
            {
                extra.push(BackendEvent::Reaction {
                    target: target.clone(),
                    by: Some(by.clone()),
                    emoji: emoji.into(),
                    remove: false,
                    at: m.sent_at + 50_000 + i64::try_from(n).unwrap_or(0) * 5_000,
                });
            }
        }
    }
    deliver_before_final_state(script, extra)
}

/// How the chat with calls is made ([`with_calls`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallsOptions {
    /// The missed call is unread (its chat has an unread count, the account's badge counts
    /// it). Off in every picture but `--page=calls`, so no other picture changes.
    pub unread: bool,
    /// The last call starts at the fixture's "now" and rings for this long (at most
    /// `kourier_model::MAX_RING_MS`: the core rings no call longer, the fixture's neither).
    /// A call that rings now is the newest thing of its account: the chat is then the first
    /// of the conversation list below the pinned ones. `None`: the last call rang long ago like the others and is
    /// a missed call; the chat keeps its place.
    pub ring_for: Option<std::time::Duration>,
}

/// Call lines in one of the generated direct chats, for `--page=calls`: the chat is the third
/// from the bottom of the conversation list (below the fold of the list in every window
/// size, and no page opens it), so no other picture shows what is added. The lines are
/// delivered after the chat's own messages and so are its newest rows; the chat keeps its
/// place in the list (a few minutes after its newest message, days before the next chat).
/// Returns the script and the chat's peer.
///
/// The chat shows, oldest first, a text and then one line of each state a call of 1:1 shows:
/// missed with a kind (a ring and the caller's hang-up), declined (video), answered on
/// another device, one made from the phone and answered (video), one made from the phone that
/// nobody answered, missed with no kind (a hang-up whose ring was not seen), and a video call
/// that rings from "now" when [`CallsOptions::ring_for`] says so (else it was missed too).
/// A call in a group is not here: a group is on the conversation list above the fold.
///
/// The calls of the past rang long before the fixture's "now": the core keeps a ring on its
/// own clock, from the receipt of the signal, for two minutes at most, so their rings are
/// missed calls the moment they arrive, and the signal that follows says what they became.
pub(crate) fn with_calls(
    script: Script,
    now_ms: i64,
    options: CallsOptions,
) -> (Script, Option<UserId>) {
    use kourier_fake::calls::Call;
    use kourier_model::CallMedia::{Video, Voice};
    let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
    let mut newest: std::collections::HashMap<UserId, i64> = std::collections::HashMap::new();
    for e in &events {
        if let BackendEvent::Message(m) = e
            && let ConvKey::Direct(peer) = &m.conv
        {
            let at = newest.entry(peer.clone()).or_insert(m.sent_at);
            *at = (*at).max(m.sent_at);
        }
    }
    let mut ranked: Vec<(UserId, i64)> = newest.into_iter().collect();
    ranked.sort_by_key(|(peer, at)| (std::cmp::Reverse(*at), peer.clone()));
    let Some((peer, last)) = ranked
        .len()
        .checked_sub(3)
        .filter(|n| *n >= 8)
        .and_then(|n| ranked.get(n).cloned())
    else {
        return (script, None);
    };
    let key = ConvKey::Direct(peer.clone());
    let base = last + 5 * 60_000;
    let at = |minutes: i64, seconds: i64| base + minutes * 60_000 + seconds * 1_000;
    let read = |mut event: BackendEvent, read: bool| {
        if let BackendEvent::Message(m) = &mut event {
            m.read = read;
        }
        event
    };
    let call = |id: u64, media| Call::direct(&peer, id, media);
    let mut text = kourier_fake::scripts::remote(
        &key,
        &peer,
        &at(0, 0).to_string(),
        at(0, 0),
        "Are you free for a call?",
    );
    text.read = true;
    let (missed, declined, taken, made, unanswered, hung_up, rings) = (
        call(7001, Voice),
        call(7002, Video),
        call(7003, Voice),
        call(7004, Video),
        call(7005, Voice),
        call(7006, Voice),
        call(7007, Video),
    );
    // The call that rings: made and received at the fixture's "now", with the time it has
    // left. Without one, a ring of the past like the others.
    let last = match options.ring_for {
        Some(ring_for) => {
            let left = i64::try_from(ring_for.as_millis()).unwrap_or(i64::MAX);
            let mut event = rings.ring_until(now_ms, now_ms.saturating_add(left));
            if let BackendEvent::Message(m) = &mut event {
                m.received_at = now_ms;
            }
            event
        }
        None => rings.ring(at(13, 0)),
    };
    let deliveries = [
        vec![BackendEvent::Message(text)],
        // The only line that can count as unread: its ring ran out long ago, which makes it
        // a missed call at once, and the hang-up that follows changes nothing of it.
        vec![read(missed.ring(at(1, 0)), !options.unread)],
        vec![read(missed.caller_gave_up(at(1, 25)), !options.unread)],
        vec![read(declined.ring(at(3, 0)), true)],
        vec![read(declined.declined(at(3, 10)), true)],
        vec![read(taken.ring(at(5, 0)), true)],
        vec![read(taken.answered_elsewhere(at(5, 20)), true)],
        vec![read(made.outgoing_answered(at(7, 0)), true)],
        vec![read(unanswered.outgoing_unanswered(at(9, 0)), true)],
        vec![read(hung_up.caller_gave_up(at(11, 0)), true)],
        vec![read(last, true)],
    ];
    let mut script = script;
    let mut last_step = None;
    if matches!(
        script.steps.last(),
        Some(Step::Conn(_) | Step::Unlink { .. })
    ) {
        last_step = script.steps.pop();
    }
    for delivery in deliveries {
        script = script.deliver(delivery);
    }
    script.steps.extend(last_step);
    (script, Some(peer))
}

/// How the page of attachments that cannot be had is made ([`with_unavailable`]).
///
/// What an import brings, in one of the generated direct chats, for `--page=unavailable`: the
/// lowest direct chat of the conversation list's body (not archived, not pinned to its top)
/// that no other page opens (not the chat of [`with_calls`], nor the one that has the
/// fixture's file and voice note, `--page=media`), below the fold of the list in every
/// window size, so no other picture shows what is added (not even a thumb of the list's
/// scroll bar: no chat is added). The messages are delivered as ONE history batch after the
/// chat's own messages, so they are its newest rows; the chat keeps its place in the list
/// (minutes after its newest message, days before the next chat). History never downloads by
/// itself, so the one picture the server may still have stays a click away.
///
/// Oldest first: an album of four, a picture, a file and a sticker "Not available on this
/// device" (the import knows the server keeps them no more), a picture and a file "No longer
/// available" (the server said so), a picture the user's phone is being asked for, a file and
/// a picture it did not answer for, a picture whose message it no longer has and a file it
/// does not have either (S6; the fixture account can ask its phone,
/// `Script::with_backfill`: the gone ones and those not answered offer Download, those being
/// asked for and the final ones do not), and a picture that downloads on a click. Returns the
/// script and the chat's peer.
pub(crate) fn with_unavailable(
    script: Script,
    calls_peer: Option<&UserId>,
) -> (Script, Option<UserId>) {
    use kourier_model::AskFailure::{NoAnswer, NoFile, NoMessage};
    use kourier_model::AttachmentState::{AskFailed, Asking, Expired, Unavailable};
    let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
    let mut newest: std::collections::HashMap<UserId, i64> = std::collections::HashMap::new();
    // The chat `with_fixture_media` puts its file and voice note into: the first with a photo.
    let mut photo_chat: Option<UserId> = None;
    // Chats that are not in the list's body: archived, or pinned to its top.
    let mut apart: std::collections::HashSet<UserId> = std::collections::HashSet::new();
    for e in &events {
        match e {
            BackendEvent::Message(m) => {
                if let ConvKey::Direct(peer) = &m.conv {
                    let at = newest.entry(peer.clone()).or_insert(m.sent_at);
                    *at = (*at).max(m.sent_at);
                    if photo_chat.is_none() && matches!(m.body, Body::Media { .. }) {
                        photo_chat = Some(peer.clone());
                    }
                }
            }
            BackendEvent::ConversationUpsert(p)
                if p.archived == Some(true) || matches!(p.pinned_order, Patch::Set(_)) =>
            {
                if let ConvKey::Direct(peer) = &p.key {
                    apart.insert(peer.clone());
                }
            }
            _ => {}
        }
    }
    let mut ranked: Vec<(UserId, i64)> = newest.into_iter().collect();
    ranked.sort_by_key(|(peer, at)| (std::cmp::Reverse(*at), peer.clone()));
    let Some((peer, last)) = ranked
        .iter()
        .enumerate()
        .rev()
        .find(|(n, (peer, _))| {
            *n >= 8
                && !apart.contains(peer)
                && Some(peer) != calls_peer
                && Some(peer) != photo_chat.as_ref()
        })
        .map(|(_, chat)| chat.clone())
    else {
        return (script, None);
    };
    let key = ConvKey::Direct(peer.clone());
    let at = |n: i64| last + 5 * 60_000 + n * 40_000;
    let gone = |kind: AttachmentKind,
                mime: &str,
                name: Option<&str>,
                dims: Option<(u32, u32)>,
                state: kourier_model::AttachmentState| Attachment {
        kind,
        mime: mime.to_owned(),
        size: 48_213,
        file_name: name.map(str::to_owned),
        dims,
        state,
        ..Attachment::default()
    };
    let picture = |state| {
        gone(
            AttachmentKind::Image,
            "image/jpeg",
            None,
            Some((200, 90)),
            state,
        )
    };
    let media = |attachments: Vec<Attachment>, caption: Option<&str>| Body::Media {
        attachments,
        caption: caption.map(Text::plain),
    };
    // (The page opens at the newest row: what fits above the composer is what the pictures
    // show, so the album, the tallest, comes first: its lower tiles, with their notes, are
    // the top of the picture.)
    let bodies = [
        media(vec![picture(Unavailable); 4], None),
        media(vec![picture(Unavailable)], None),
        media(
            vec![gone(
                AttachmentKind::File,
                "application/pdf",
                Some("flight-plan.pdf"),
                None,
                Unavailable,
            )],
            None,
        ),
        Body::Sticker(Box::new(gone(
            AttachmentKind::Image,
            "image/webp",
            None,
            Some((512, 512)),
            Unavailable,
        ))),
        media(vec![picture(Expired)], None),
        media(
            vec![gone(
                AttachmentKind::File,
                "application/gpx+xml",
                Some("ridge-track.gpx"),
                None,
                Expired,
            )],
            None,
        ),
        // The user's phone asked for a lost file (S6): still asking, then each way it ends.
        media(vec![picture(Asking)], None),
        media(
            vec![gone(
                AttachmentKind::File,
                "application/pdf",
                Some("checklist.pdf"),
                None,
                AskFailed { why: NoAnswer },
            )],
            None,
        ),
        media(vec![picture(AskFailed { why: NoAnswer })], None),
        media(vec![picture(AskFailed { why: NoMessage })], None),
        media(
            vec![gone(
                AttachmentKind::File,
                "application/zip",
                Some("logs.zip"),
                None,
                AskFailed { why: NoFile },
            )],
            None,
        ),
        media(vec![synthetic_image(200, 90, 61)], None),
    ];
    let messages = bodies
        .into_iter()
        .enumerate()
        .map(|(n, body)| {
            let n = i64::try_from(n).unwrap_or(0);
            let mut m = kourier_fake::scripts::remote(&key, &peer, &at(n).to_string(), at(n), "");
            m.body = body;
            m.read = true;
            m
        })
        .collect();
    let script = deliver_before_final_state(
        script,
        vec![BackendEvent::HistoryBatch {
            conv: key,
            messages,
            has_more: false,
            progress: None,
        }],
    );
    (script, Some(peer))
}

/// Adds the fixture's media to `script`: a file and a voice note after the newest message of
/// the conversation with the first photo, and profile pictures for the first contacts and
/// for the account itself.
pub(crate) fn with_fixture_media(script: Script) -> (Script, FixtureAvatars) {
    let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
    let mut extra = Vec::new();
    let mut avatars = FixtureAvatars::default();
    // Our own profile picture (the drawer's account entry): the contact of our first id.
    if let Some(me) = script.own_ids.first() {
        let rel = PathBuf::from("fixture/avatar-own");
        let key = FileKey::generate();
        let mut patch = ContactPatch::new(me.clone());
        patch.avatar = Patch::Set(BlobRef {
            path: rel.clone(),
            key: BlobKey::new(*key.as_bytes()),
        });
        extra.push(BackendEvent::ContactUpsert(patch));
        avatars.files.push((rel, key, portrait(AVATARS + 3)));
    }
    let contacts = events.iter().filter_map(|e| match e {
        BackendEvent::ContactUpsert(p) if matches!(p.profile_name, Patch::Set(_)) => Some(&p.user),
        _ => None,
    });
    for (n, user) in contacts.take(AVATARS).enumerate() {
        let rel = PathBuf::from(format!("fixture/avatar-{n}"));
        let key = FileKey::generate();
        let mut patch = ContactPatch::new(user.clone());
        patch.avatar = Patch::Set(BlobRef {
            path: rel.clone(),
            key: BlobKey::new(*key.as_bytes()),
        });
        extra.push(BackendEvent::ContactUpsert(patch));
        avatars.files.push((rel, key, portrait(n)));
    }
    let photo = events.iter().find_map(|e| match e {
        BackendEvent::Message(m) if matches!(m.body, Body::Media { .. }) => Some(m.clone()),
        _ => None,
    });
    if let Some(photo) = photo {
        let newest = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if m.conv == photo.conv => Some(m.sent_at),
                _ => None,
            })
            .max()
            .unwrap_or(photo.sent_at);
        let message = |n: i64, att: kourier_model::Attachment| {
            let at = newest + n * 45_000;
            let mut m = IncomingMessage::new(
                photo.conv.clone(),
                format!("{at}").as_str(),
                photo.origin.clone(),
                Body::Media {
                    attachments: vec![att],
                    caption: None,
                },
                at,
                at + 500,
            );
            m.read = true;
            BackendEvent::Message(m)
        };
        let mut voice = synthetic_file("voice.ogg", "audio/ogg", 9_216, 7);
        voice.kind = AttachmentKind::Voice;
        voice.file_name = None;
        voice.duration_ms = Some(7_000);
        extra.push(message(
            1,
            synthetic_file("Flight plan.pdf", "application/pdf", 182_404, 5),
        ));
        extra.push(message(2, voice));
    }
    (deliver_before_final_state(script, extra), avatars)
}

/// A soft portrait silhouette on a gradient, in a hue of its own.
fn portrait(n: usize) -> Vec<u8> {
    let hue = (n as f32 * 0.161 + 0.05).fract();
    let side = AVATAR_SIDE as f32;
    png(AVATAR_SIDE, AVATAR_SIDE, |x, y| {
        let (fx, fy) = (x as f32 / side, y as f32 / side);
        let head = ((fx - 0.5).powi(2) + (fy - 0.4).powi(2)).sqrt() < 0.19;
        let body = ((fx - 0.5) / 0.36).powi(2) + ((fy - 1.02) / 0.34).powi(2) < 1.0;
        let light = if head || body {
            0.86
        } else {
            0.42 + 0.22 * (fx + fy) / 2.0
        };
        hsl(hue, if head || body { 0.25 } else { 0.55 }, light)
    })
}

fn hsl(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h * 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    [to(r), to(g), to(b)]
}

/// A truecolor PNG (stored deflate blocks: no compressor needed for 96 × 96 pixels).
fn png(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
    for y in 0..height {
        raw.push(0); // filter: none
        for x in 0..width {
            raw.extend_from_slice(&pixel(x, y));
        }
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks = raw.chunks(65_535).collect::<Vec<_>>();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &zlib), (b"IEND", &Vec::new())] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &d in data {
        a = (a + u32::from(d)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        let p = portrait(0);
        assert!(p.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(p.ends_with(&[0xae, 0x42, 0x60, 0x82]), "IEND CRC");
    }

    #[test]
    fn the_fixture_gets_media_and_avatars() {
        let script = kourier_fake::fixture::fixture(&kourier_fake::fixture::FixtureSpec {
            messages_per_conversation: 10,
            ..Default::default()
        })
        .expect("fixture");
        let before = script.deliveries().len();
        let (script, avatars) = with_fixture_media(script);
        assert_eq!(script.deliveries().len(), before + 1);
        // The contacts' pictures and our own.
        assert_eq!(avatars.files.len(), AVATARS + 1);
        let dir = tempfile::tempdir().expect("dir");
        avatars.write(dir.path()).expect("write");
        let (rel, key, png) = &avatars.files[0];
        let back = kourier_blob::read_all(&dir.path().join(rel), key).expect("read");
        assert_eq!(back.as_slice(), png.as_slice());
    }

    #[test]
    fn extras_are_delivered_before_the_account_changes_state() {
        use kourier_fake::fixture::{FixtureSpec, FixtureState, fixture};
        let script = fixture(&FixtureSpec {
            messages_per_conversation: 10,
            showcase: true,
            state: FixtureState::LoggedOut,
            ..Default::default()
        })
        .expect("fixture");
        let newest = newest_message(&script).expect("messages");
        let (script, _) = with_fixture_media(with_showcase_extras(script));
        // The unlink stays the last step: nothing could be delivered after it, and the fake
        // counts the script as finished only then.
        assert!(matches!(script.steps.last(), Some(Step::Unlink { .. })));
        let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
        let kinds = kinds_peer().expect("peer");
        let hints: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if m.conv == ConvKey::Direct(kinds.clone()) => {
                    match &m.body {
                        Body::Unsupported { kind_hint, .. } => Some(kind_hint.as_str()),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        assert!(
            hints.contains(&"view-once") && hints.contains(&"payment"),
            "{hints:?}"
        );
        // Several members react to one own message of a group.
        let reactions = events
            .iter()
            .filter(|e| {
                matches!(e, BackendEvent::Reaction { by: Some(_), target, .. }
                if matches!(target.conv, Some(ConvKey::Group(_))))
            })
            .count();
        assert!(reactions >= 4, "{reactions}");
        // The extras do not move the fixture's "now".
        assert_eq!(newest_message(&script), Some(newest));
        // The albums chat: two, three, four and nine pictures, and a text edited twice.
        let albums = albums_peer().expect("peer");
        let mut counts: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if m.conv == ConvKey::Direct(albums.clone()) => {
                    match &m.body {
                        Body::Media { attachments, .. } => Some(attachments.len()),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, [2, 3, 4, 9]);
        let edits = events
            .iter()
            .filter(|e| {
                matches!(e, BackendEvent::Edited { target, .. }
                if target.conv == Some(ConvKey::Direct(albums.clone())))
            })
            .count();
        assert_eq!(edits, 2);
        // The long chat: one message, longer than the timeline shows at first ("Read more"
        // starts at 900 characters) and shorter than one step adds.
        let long = readmore_peer().expect("peer");
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if m.conv == ConvKey::Direct(long.clone()) => {
                    match &m.body {
                        Body::Text(t) => Some(t.body.as_str()),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect();
        assert_eq!(texts, [LONG_TEXT]);
        assert!((901..3800).contains(&LONG_TEXT.chars().count()));
        // The scripted timer versions stay as the fake wrote them: its own timer changes
        // count on from them.
        assert!(events.iter().any(|e| match e {
            BackendEvent::ConversationUpsert(p) => p.timer_version.is_some(),
            _ => false,
        }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, BackendEvent::ConversationUpsert(p)
                if matches!(p.timer, Patch::Set(_)))),
            "the timer chat keeps its timer"
        );
    }
}
