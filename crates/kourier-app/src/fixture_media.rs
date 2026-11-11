//! Media of the fixture data set (`--fixture`, §10.2): a file and a voice note in the
//! conversation with the first photo, and profile pictures for the first contacts, so the
//! screenshots show the attachment blocks and round avatars. The fake network delivers the
//! messages and the contacts' avatar references; the avatar files are written into the
//! account's blob directory (encrypted, like a backend writes them) once the account exists.

use std::path::{Path, PathBuf};

use kourier_backend_api::{BackendEvent, BackendNotice, IncomingMessage};
use kourier_blob::FileKey;
use kourier_fake::attachments::{synthetic_file, synthetic_image, synthetic_image_claiming};
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

/// The lowest direct chat of the conversation list's body (not archived, not pinned to its
/// top, the ninth from the newest at the highest) that no other page opens: not one of
/// `taken` (the chats of [`with_calls`] and [`with_unavailable`]), nor the one that has the
/// fixture's file and voice note (`--page=media`). It is below the fold of the list in every
/// window size, so what a page adds to it moves no other picture (not even a thumb of the
/// list's scroll bar: no chat is added). Returns its peer and the time of its newest message.
fn lowest_plain_chat(script: &Script, taken: &[Option<&UserId>]) -> Option<(UserId, i64)> {
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
    ranked
        .iter()
        .enumerate()
        .rev()
        .find(|(n, (peer, _))| {
            *n >= 8
                && !apart.contains(peer)
                && !taken.contains(&Some(peer))
                && Some(peer) != photo_chat.as_ref()
        })
        .map(|(_, chat)| chat.clone())
}

/// How many groups the contact of `--page=person` shares with the account.
pub(crate) const PERSON_GROUPS: usize = 3;

/// The safety numbers of the person of `--page=person` (P5a): one synthetic identity key
/// (33 bytes, the type byte 0x05 first, as a real one), state Default; with `changed` (the page
/// `safetychanged`) the same key in state Changed (verified once, then the key changed).
pub(crate) fn person_safety(
    changed: bool,
) -> Vec<(
    kourier_model::IdentityFingerprint,
    kourier_model::VerifyState,
)> {
    let mut key = vec![0x05];
    key.extend((1..=32u8).map(|i| i.wrapping_mul(37)));
    vec![(
        kourier_model::IdentityFingerprint(key),
        if changed {
            kourier_model::VerifyState::Changed
        } else {
            kourier_model::VerifyState::Default
        },
    )]
}

/// The safety numbers of the second person of the safety steps (`FixtureOptions::safety_peer`,
/// P5a critic F15): two synthetic keys, both in state Changed (she was verified, then her key
/// changed): the first answer shows the first key and the fake holds the second at once, so a
/// Mark of the shown number is refused (the page is stale) and the re-read shows the second,
/// which Confirm then accepts.
pub(crate) fn safety_peer_keys() -> Vec<(
    kourier_model::IdentityFingerprint,
    kourier_model::VerifyState,
)> {
    [11u8, 23u8]
        .into_iter()
        .map(|m| {
            let mut key = vec![0x05];
            key.extend((1..=32u8).map(|i| i.wrapping_mul(m).wrapping_add(m)));
            (
                kourier_model::IdentityFingerprint(key),
                kourier_model::VerifyState::Changed,
            )
        })
        .collect()
}

/// The second person of the safety steps (`FixtureOptions::safety_peer`, P5a critic F15): the
/// lowest plain chat no other page takes (no chat added). She gets [`safety_peer_keys`] and the
/// first send into her chat fails `UntrustedIdentity` (one attempt), so that chat pauses until
/// Confirm; nothing else of the script changes. Returns the script and her peer id.
pub(crate) fn with_safety_peer(
    script: Script,
    taken: &[Option<&UserId>],
) -> (Script, Option<UserId>) {
    let Some((peer, _)) = lowest_plain_chat(&script, taken) else {
        return (script, None);
    };
    let script = script
        .with_safety(peer.clone(), safety_peer_keys())
        .fail_transmit(
            kourier_fake::TransmitRule::attempts(
                kourier_model::BackendError::new(
                    kourier_model::ErrorKind::UntrustedIdentity { user: peer.clone() },
                    "identity changed",
                ),
                1,
            )
            .in_conv(ConvKey::Direct(peer.clone())),
        );
    (script, Some(peer))
}

/// The third person of the safety steps (`FixtureOptions::safety_peer`, P5a U6): the next lowest
/// plain chat (no chat added), one synthetic key in state Default (never verified), and the
/// first send into her chat fails `UntrustedIdentity` (one attempt), so that chat pauses until
/// Confirm, which for a person never verified is an approval. The fake learns her key late (Y1):
/// the first ask for her number is answered "no key yet", the next ones have it, so a level
/// opened before anything is sent shows a REAL answer without a number, and the number follows
/// when the send pauses. Returns the script and her peer.
pub(crate) fn with_safety_default_peer(
    script: Script,
    taken: &[Option<&UserId>],
) -> (Script, Option<UserId>) {
    let Some((peer, _)) = lowest_plain_chat(&script, taken) else {
        return (script, None);
    };
    let mut key = vec![0x05];
    key.extend((1..=32u8).map(|i| i.wrapping_mul(53).wrapping_add(7)));
    let script = script
        .with_late_safety(
            peer.clone(),
            vec![(
                kourier_model::IdentityFingerprint(key),
                kourier_model::VerifyState::Default,
            )],
            1,
        )
        .fail_transmit(
            kourier_fake::TransmitRule::attempts(
                kourier_model::BackendError::new(
                    kourier_model::ErrorKind::UntrustedIdentity { user: peer.clone() },
                    "identity changed",
                ),
                1,
            )
            .in_conv(ConvKey::Direct(peer.clone())),
        );
    (script, Some(peer))
}

/// The person of `--page=person` (people pages P1): the generated contact who shares the most
/// groups with the account (three or more with the default fixture), found among the chats the
/// script already makes, so no chat is added and nothing the list shows moves. She gets an about
/// text with its emoji and a phone number she shares, as ONE contact update delivered with the
/// rest of the data (it draws nothing from the fixture's random stream). Returns the script and
/// her peer id; `taken` are the peers other pages use.
pub(crate) fn with_person(
    script: Script,
    taken: &[Option<&UserId>],
    groups_wanted: usize,
) -> (Script, Option<UserId>) {
    use kourier_model::{Identifier, IdentifierKind};
    let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
    let mut direct: Vec<UserId> = Vec::new();
    let mut archived: std::collections::HashSet<UserId> = std::collections::HashSet::new();
    let mut groups: std::collections::HashMap<UserId, usize> = std::collections::HashMap::new();
    for e in &events {
        let BackendEvent::ConversationUpsert(p) = e else {
            continue;
        };
        match &p.key {
            ConvKey::Direct(peer) => {
                if !direct.contains(peer) {
                    direct.push(peer.clone());
                }
                if p.archived == Some(true) {
                    archived.insert(peer.clone());
                }
            }
            ConvKey::Group(_) => {
                for m in p.members.iter().flat_map(|l| &l.upsert) {
                    *groups.entry(m.user.clone()).or_insert(0) += 1;
                }
            }
            _ => {}
        }
    }
    let best = direct
        .into_iter()
        .filter(|u| !archived.contains(u) && !taken.contains(&Some(u)))
        .filter_map(|u| groups.get(&u).map(|n| (*n, u)))
        .max_by(|(an, au), (bn, bu)| an.cmp(bn).then_with(|| bu.cmp(au)));
    let Some((_, peer)) = best else {
        return (script, None);
    };
    // She shares three groups with the account: the generated ones give the top contacts a few,
    // and the chats other pages use are taken. Missing ones are made by a member delta of the
    // OLDEST groups (no chat is added); the group with the newest message, the one the `chat` page
    // opens, keeps its members, so no picture of another page moves.
    let mut newest: std::collections::HashMap<ConvKey, i64> = std::collections::HashMap::new();
    for e in &events {
        if let BackendEvent::Message(m) = e
            && matches!(m.conv, ConvKey::Group(_))
        {
            let at = newest.entry(m.conv.clone()).or_insert(m.sent_at);
            *at = (*at).max(m.sent_at);
        }
    }
    let skip = newest
        .iter()
        .max_by_key(|(_, at)| **at)
        .map(|(k, _)| k.clone());
    let mut hers = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) if matches!(p.key, ConvKey::Group(_)) => p
                .members
                .as_ref()
                .filter(|l| l.upsert.iter().any(|m| m.user == peer)),
            _ => None,
        })
        .count();
    let mut oldest_first: Vec<(&ConvKey, i64)> = newest.iter().map(|(k, at)| (k, *at)).collect();
    oldest_first.sort_by_key(|(k, at)| (*at, format!("{k:?}")));
    let mut joins = Vec::new();
    for (key, _) in oldest_first {
        if hers >= groups_wanted || Some(key) == skip.as_ref() {
            continue;
        }
        let member_already = events.iter().any(|e| {
            matches!(e, BackendEvent::ConversationUpsert(p) if p.key == *key
                && p.members.as_ref().is_some_and(|l| l.upsert.iter().any(|m| m.user == peer)))
        });
        if member_already {
            continue;
        }
        let mut patch = ConversationPatch::new(key.clone());
        patch.members = Some(kourier_model::MemberList {
            full: false,
            upsert: vec![kourier_model::Member {
                user: peer.clone(),
                role: kourier_model::MemberRole::Member,
            }],
            remove: Vec::new(),
        });
        joins.push(BackendEvent::ConversationUpsert(patch));
        hers += 1;
    }
    // A test that wants more groups than the fixture has gets groups of their own (chats more,
    // so never a fixture page: its list would move). Each has one message.
    if hers < groups_wanted
        && let (Some(me), Some(newest)) = (script.own_ids.first().cloned(), newest_message(&script))
    {
        for n in 0..groups_wanted - hers {
            let key = ConvKey::Group(format!("gv2:ZXh0cmEtcGVyc29uLWdyb3Vw{n:03}").into());
            let mut patch = ConversationPatch::new(key.clone());
            patch.title = Patch::Set(format!("Extra group {}", n + 1));
            patch.members = Some(kourier_model::MemberList::snapshot(vec![
                kourier_model::Member {
                    user: peer.clone(),
                    role: kourier_model::MemberRole::Admin,
                },
                kourier_model::Member {
                    user: me.clone(),
                    role: kourier_model::MemberRole::Member,
                },
            ]));
            patch.features = Some(kourier_fake::scripts::signal_features(true));
            let at = newest - 120_000 - i64::try_from(n).unwrap_or(0) * 1_000;
            joins.push(BackendEvent::ConversationUpsert(patch));
            joins.push(BackendEvent::Message(kourier_fake::scripts::own(
                &key,
                &format!("extra-group-{n}"),
                at,
                "A message in an extra group.",
            )));
        }
    }
    let mut patch = ContactPatch::new(peer.clone());
    patch.about = Patch::Set(
        "Flying for the view, building for the fun of it: https://example.org/pilot".into(),
    );
    patch.about_emoji = Patch::Set("\u{1F6EB}".into());
    patch.number_shown = Some(true);
    patch.identifiers = Some(vec![Identifier {
        kind: IdentifierKind::Phone,
        value: "+15550173642".into(),
    }]);
    joins.push(BackendEvent::ContactUpsert(patch));
    let script = deliver_before_final_state(script, joins);
    (script, Some(peer))
}

/// The description of the group of `--page=group`: two sentences, long enough to be cut at three
/// lines in the narrow panel and shown whole in the wide one, with one address in the first line
/// (the link's look is in the picture, F1; `chat_ui` follows it).
pub(crate) const GROUP_DESCRIPTION: &str = "Plan the next meeting at https://example.org/regulars and share what you found along the way. Be kind, stay on topic, and keep the photos coming.";

/// The profile name of the synthetic member that has a contact row in a grown group
/// ([`with_group`] with `members_wanted`): markup, to be shown as the plain text it is.
pub(crate) const GROUP_MARKUP_NAME: &str = "<b>Bold</b> name";

/// The group of `--page=group`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FixtureGroup {
    /// Its key in the script.
    pub key: ConvKey,
    /// Its title (the page finds the group's row by it).
    pub title: String,
    /// Its full members, other than the account, who have a contact row (the senders of
    /// [`with_group_mentions`]), in the order of their ids.
    pub named: Vec<UserId>,
}

/// The group of `--page=group` (people pages P2): the group with the most members that is not the
/// one with the newest message (the `chat` page opens that one; its members and features stay as
/// they are), the oldest of equals, found among the chats the script already makes, so no chat is
/// added and nothing the list or another page shows moves. It gets a description, the rules of a
/// Signal group the account is no admin of (only admins change the group info; Signal's group
/// rule: no timer to set here) and ONE invited member, as ONE conversation upsert and one contact
/// delivered with the rest of the data (nothing is drawn from the fixture's random stream).
/// `taken` are the peers other pages use (never the invited one). With `members_wanted` > 0 (a
/// test, never a fixture page: it changes the members of that group only) the group grows to
/// that many members and members: existing contacts first, but always two made-up ones, one with
/// a contact row whose name is markup ([`GROUP_MARKUP_NAME`]) and one without a contact row
/// (the fallback name).
pub(crate) fn with_group(
    script: Script,
    taken: &[Option<&UserId>],
    members_wanted: usize,
) -> (Script, Option<FixtureGroup>) {
    use kourier_model::{Member, MemberList, MemberRole};
    use std::collections::BTreeMap;
    let Some(me) = script.own_ids.first().cloned() else {
        return (script, None);
    };
    let events: Vec<BackendEvent> = script.deliveries().into_iter().flatten().collect();
    struct Seen {
        key: ConvKey,
        title: Option<String>,
        roles: BTreeMap<UserId, MemberRole>,
        newest: Option<i64>,
    }
    let mut seen: Vec<Seen> = Vec::new();
    let mut contacts: Vec<UserId> = Vec::new();
    for e in &events {
        match e {
            BackendEvent::ContactUpsert(p) => {
                if p.user != me && !contacts.contains(&p.user) {
                    contacts.push(p.user.clone());
                }
            }
            BackendEvent::ConversationUpsert(p) if matches!(p.key, ConvKey::Group(_)) => {
                let at = match seen.iter().position(|g| g.key == p.key) {
                    Some(at) => at,
                    None => {
                        seen.push(Seen {
                            key: p.key.clone(),
                            title: None,
                            roles: BTreeMap::new(),
                            newest: None,
                        });
                        seen.len() - 1
                    }
                };
                let g = &mut seen[at];
                if let Patch::Set(title) = &p.title {
                    g.title = Some(title.clone());
                }
                if let Some(list) = &p.members {
                    if list.full {
                        g.roles.clear();
                    }
                    for m in &list.upsert {
                        g.roles.insert(m.user.clone(), m.role);
                    }
                    for gone in &list.remove {
                        g.roles.remove(gone);
                    }
                }
            }
            BackendEvent::Message(m) if matches!(m.conv, ConvKey::Group(_)) => {
                if let Some(g) = seen.iter_mut().find(|g| g.key == m.conv) {
                    g.newest = g.newest.max(Some(m.sent_at));
                }
            }
            _ => {}
        }
    }
    // (A message can come before the group's first upsert in a script of a test: look again.)
    for e in &events {
        if let BackendEvent::Message(m) = e
            && let Some(g) = seen.iter_mut().find(|g| g.key == m.conv)
        {
            g.newest = g.newest.max(Some(m.sent_at));
        }
    }
    let newest_key = seen
        .iter()
        .max_by_key(|g| (g.newest, format!("{:?}", g.key)))
        .map(|g| g.key.clone());
    let full = |g: &Seen| {
        g.roles
            .values()
            .filter(|r| matches!(r, MemberRole::Member | MemberRole::Admin))
            .count()
    };
    let Some(pick) = seen
        .iter()
        .filter(|g| Some(&g.key) != newest_key.as_ref())
        .max_by_key(|g| {
            (
                full(g),
                std::cmp::Reverse(g.newest),
                std::cmp::Reverse(format!("{:?}", g.key)),
            )
        })
    else {
        return (script, None);
    };
    let title = pick.title.clone().unwrap_or_default();
    let mut roles = pick.roles.clone();
    let mut delta: Vec<Member> = Vec::new();
    let mut extra: Vec<BackendEvent> = Vec::new();
    let mut extra_named: Vec<UserId> = Vec::new();
    // One invited member: an existing contact who is in no other role here and whom no other
    // page shows.
    contacts.sort();
    if let Some(invited) = contacts
        .iter()
        .find(|u| !roles.contains_key(*u) && !taken.contains(&Some(*u)))
        .cloned()
    {
        roles.insert(invited.clone(), MemberRole::Pending);
        delta.push(Member {
            user: invited,
            role: MemberRole::Pending,
        });
    }
    if members_wanted > 0 {
        let made_up = |n: u64| {
            UserId::parse(&format!(
                "aci:00000000-0000-4000-8000-{:012x}",
                0x00f4_0000 + n
            ))
            .ok()
        };
        let add =
            |roles: &mut BTreeMap<UserId, MemberRole>, delta: &mut Vec<Member>, user: UserId| {
                roles.insert(user.clone(), MemberRole::Member);
                delta.push(Member {
                    user,
                    role: MemberRole::Member,
                });
            };
        // A made-up member WITH a contact row whose name is markup, one WITHOUT a contact row.
        if let Some(marked) = made_up(1) {
            extra_named.push(marked.clone());
            let mut contact = ContactPatch::new(marked.clone());
            contact.profile_name = Patch::Set(GROUP_MARKUP_NAME.into());
            extra.push(BackendEvent::ContactUpsert(contact));
            add(&mut roles, &mut delta, marked);
        }
        if let Some(bare) = made_up(2) {
            add(&mut roles, &mut delta, bare);
        }
        let have = |roles: &BTreeMap<UserId, MemberRole>| {
            roles
                .values()
                .filter(|r| matches!(r, MemberRole::Member | MemberRole::Admin))
                .count()
        };
        // The people other pages use come first, so a test can walk from the group to a
        // person whose chat it knows (the person page's person: her chat has a timer).
        for user in taken.iter().flatten() {
            if contacts.contains(user) && !roles.contains_key(*user) {
                add(&mut roles, &mut delta, (*user).clone());
            }
        }
        for user in &contacts {
            if have(&roles) >= members_wanted {
                break;
            }
            if !roles.contains_key(user) {
                add(&mut roles, &mut delta, user.clone());
            }
        }
        // More than the contacts give: made-up members without a contact row.
        let mut n = 3;
        while have(&roles) < members_wanted {
            let Some(user) = made_up(n) else { break };
            add(&mut roles, &mut delta, user);
            n += 1;
        }
    }
    let mut patch = ConversationPatch::new(pick.key.clone());
    patch.description = Patch::Set(GROUP_DESCRIPTION.into());
    let mut features = kourier_fake::scripts::signal_features(true);
    // Signal's groups offer no timer to this client; the account is a plain member of a group
    // whose info only admins change.
    features.disappearing.clear();
    features.disappearing_custom.clear();
    features.group_info = Some(kourier_model::GroupInfo {
        attributes_admin_only: true,
        ..Default::default()
    });
    patch.features = Some(features);
    if !delta.is_empty() {
        patch.members = Some(MemberList {
            full: false,
            upsert: delta,
            remove: Vec::new(),
        });
    }
    let named: Vec<UserId> = roles
        .iter()
        .filter(|(u, r)| {
            matches!(r, MemberRole::Member | MemberRole::Admin)
                && **u != me
                && (contacts.contains(*u) || extra_named.contains(*u))
        })
        .map(|(u, _)| u.clone())
        .collect();
    let found = FixtureGroup {
        key: pick.key.clone(),
        title,
        named,
    };
    let mut joins = extra;
    joins.push(BackendEvent::ConversationUpsert(patch));
    (deliver_before_final_state(script, joins), Some(found))
}

/// Ten messages more in the group of `--page=group`, the newest of it
/// (`FixtureOptions::group_mentions`, the tests of the senders and mentions that open a person: P3,
/// and of the links a text makes by itself: LD; never a fixture page, the group would become the
/// newest chat). Two members write, A (`prefer` when she is a member, else the first) and B (the next
/// one); the texts begin with `P3 `:
/// - `P3 bare` (A): an address without a scheme and an e-mail address;
/// - `P3 turned` and `P3 below` (A): a bidi override left open before a link, on the link's line and
///   on the line above it;
/// - `P3 lead` (B), `P3 run 1` and `P3 run 2` (A): a run of two by the same author (the name on the
///   first, the picture on the second);
/// - `P3 welcome` (B): a mention of A and a mention of the account;
/// - `P3 forged` (A): a mention of B, and the text `kourier:mention/0` made a link by a range;
/// - `P3 hidden` (B): a mention of A under a spoiler;
/// - `P3 linked` (A): a mention of B inside a link.
pub(crate) fn with_group_mentions(
    script: Script,
    group: &FixtureGroup,
    prefer: Option<&UserId>,
) -> Script {
    use kourier_model::text::MENTION_PLACEHOLDER;
    use kourier_model::{RangeKind, TextRange};
    let Some(me) = script.own_ids.first().cloned() else {
        return script;
    };
    let Some(newest) = newest_message(&script) else {
        return script;
    };
    let a = prefer
        .filter(|u| group.named.contains(u))
        .or(group.named.first())
        .cloned();
    let b = group.named.iter().find(|u| Some(*u) != a.as_ref()).cloned();
    let (Some(a), Some(b)) = (a, b) else {
        return script;
    };
    let m = MENTION_PLACEHOLDER;
    // The byte range of the `n`-th `needle` of `body`.
    let range = |body: &str, needle: &str, kind: RangeKind| {
        let start = body.find(needle).unwrap_or(0);
        TextRange {
            start: start as u32,
            len: needle.len() as u32,
            kind,
        }
    };
    let mention = |body: &str, nth: usize, user: &UserId| {
        let start = body.match_indices(m).nth(nth).map_or(0, |(i, _)| i);
        TextRange {
            start: start as u32,
            len: m.len_utf8() as u32,
            kind: RangeKind::Mention(user.clone()),
        }
    };
    let mut messages = Vec::new();
    let mut add = |author: &UserId, local: &str, body: String, ranges: Vec<TextRange>| {
        let at = newest + 1000 * (messages.len() as i64 + 1);
        let mut message = kourier_fake::scripts::remote(&group.key, author, local, at, "");
        message.body = Body::Text(Text { body, ranges });
        messages.push(BackendEvent::Message(message));
    };
    // LD: A writes them, so the run of two below stays what it was, and they come first, so the rows
    // of the people's steps stand where they stood.
    add(
        &a,
        "ld-bare",
        "P3 bare example.org/fpv and pilot@example.net here".to_owned(),
        Vec::new(),
    );
    add(
        &a,
        "ld-turned",
        "P3 turned \u{202E}see https://example.invalid/turned".to_owned(),
        Vec::new(),
    );
    add(
        &a,
        "ld-below",
        "P3 below \u{202E}see\nhttps://example.invalid/below".to_owned(),
        Vec::new(),
    );
    // A run of two messages by A, led by one of B's (so A's first one shows the name and not the
    // picture, her second shows the picture and not the name).
    add(&b, "p3-lead", "P3 lead".to_owned(), Vec::new());
    add(&a, "p3-run-1", "P3 run 1".to_owned(), Vec::new());
    add(&a, "p3-run-2", "P3 run 2".to_owned(), Vec::new());
    let body = format!("P3 welcome {m} and {m}, glad you are here");
    let ranges = vec![mention(&body, 0, &a), mention(&body, 1, &me)];
    add(&b, "p3-welcome", body, ranges);
    let body = format!("P3 forged {m} and kourier:mention/0 here");
    let ranges = vec![
        mention(&body, 0, &b),
        range(
            &body,
            "kourier:mention/0",
            RangeKind::Link("kourier:mention/0".into()),
        ),
    ];
    add(&a, "p3-forged", body, ranges);
    let body = format!("P3 hidden {m} secret");
    let ranges = vec![
        mention(&body, 0, &a),
        TextRange {
            start: body.find(m).unwrap_or(0) as u32,
            len: m.len_utf8() as u32,
            kind: RangeKind::Spoiler,
        },
    ];
    add(&b, "p3-hidden", body, ranges);
    let body = format!("P3 linked {m} inside");
    let ranges = vec![
        mention(&body, 0, &b),
        TextRange {
            start: body.find(m).unwrap_or(0) as u32,
            len: m.len_utf8() as u32,
            kind: RangeKind::Link("https://example.invalid/mention-link".into()),
        },
    ];
    add(&a, "p3-linked", body, ranges);
    deliver_before_final_state(script, messages)
}

/// The group of the group page offers the disappearing timers and every member may change them
/// (`FixtureOptions::group_timers`, for the test that changes the page for real).
pub(crate) fn with_group_timers(script: Script, key: &ConvKey) -> Script {
    let mut patch = ConversationPatch::new(key.clone());
    // (`signal_features(true)` states a group nobody restricts and offers the timers.)
    patch.features = Some(kourier_fake::scripts::signal_features(true));
    deliver_before_final_state(script, vec![BackendEvent::ConversationUpsert(patch)])
}

/// The account is an administrator of the group of the group page
/// (`FixtureOptions::group_admin`, for the invite link's part of the page).
pub(crate) fn with_group_admin(script: Script, key: &ConvKey) -> Script {
    let Some(me) = script.own_ids.first().cloned() else {
        return script;
    };
    let mut patch = ConversationPatch::new(key.clone());
    patch.members = Some(kourier_model::MemberList {
        full: false,
        upsert: vec![kourier_model::Member {
            user: me,
            role: kourier_model::MemberRole::Admin,
        }],
        remove: Vec::new(),
    });
    deliver_before_final_state(script, vec![BackendEvent::ConversationUpsert(patch)])
}

/// A Note to Self chat with one message (`FixtureOptions::note_to_self`, for the tests of the
/// user's own page): the account's own id as a direct chat. It is a chat more, so the list's
/// pictures move: never on a fixture page. Returns the script and the chat's peer (the user).
pub(crate) fn with_note_to_self(script: Script) -> (Script, Option<UserId>) {
    let Some(me) = script.own_ids.first().cloned() else {
        return (script, None);
    };
    let Some(newest) = newest_message(&script) else {
        return (script, None);
    };
    let key = ConvKey::Direct(me.clone());
    let mut patch = ConversationPatch::new(key.clone());
    patch.features = Some(kourier_fake::scripts::signal_features(false));
    let at = newest - 60_000;
    let message = kourier_fake::scripts::own(&key, "note-to-self", at, "A reminder to myself.");
    let script = deliver_before_final_state(
        script,
        vec![
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(message),
        ],
    );
    (script, Some(me))
}

/// The uploads of [`FixtureSends::Uploading`](crate::app::FixtureSends::Uploading): the messages
/// with files sent into the lowest plain chat (the one `--page=walock` uses: below the fold of the
/// list in every window size, no chat is added) upload slowly and report each file at a fixed
/// share, repeated every second, for as long as a slow send lasts. Returns the script and the
/// chat's peer.
///
/// The shares: [`FixtureUpload::Album`](crate::app::FixtureUpload::Album) has seven pictures (the
/// first is up, the second at 45 %, the rest wait, so the fifth tile is the "+2" ring),
/// `File` and `Late` one file at 40 % (`Late`'s network answers a stop `TooLate`), `Idle` every
/// file at 30 % (a test or the person sends what he likes).
pub(crate) fn with_uploads(
    script: Script,
    upload: crate::app::FixtureUpload,
    taken: &[Option<&UserId>],
) -> (Script, Option<UserId>) {
    use crate::app::FixtureUpload::{Album, File, Idle, Late};
    let Some((peer, _)) = lowest_plain_chat(&script, taken) else {
        return (script, None);
    };
    let shares = match upload {
        Album => vec![1.0, 0.45, 0.0, 0.0, 0.0, 0.0, 0.0],
        File | Late => vec![0.4],
        Idle => vec![0.3; 40],
    };
    let hold = crate::app::FixtureSends::SLOW;
    let held = if upload == Late {
        kourier_fake::UploadScript::TooLate { shares, hold }
    } else {
        kourier_fake::UploadScript::Held { shares, hold }
    };
    // The chat takes an album of Signal's size (the fake's own rule is three pictures a message,
    // so seven pictures would be three messages and no tile could be a "+2").
    let key = ConvKey::Direct(peer.clone());
    let mut features = kourier_fake::scripts::signal_features(false);
    for album in &mut features.file_packing.albums {
        album.max = 31;
    }
    let mut patch = ConversationPatch::new(key.clone());
    patch.features = Some(features);
    let script = deliver_before_final_state(script, vec![BackendEvent::ConversationUpsert(patch)])
        .with_uploads(key, held);
    (script, Some(peer))
}

/// Writes the files the pages `uploading` and `uploadfile` send into `dir` and returns their
/// paths: the seven pictures of the album (each its own hue, a gradient with a bright disc,
/// 800 by 600 pixels) or the file (30 MB of bytes that do not compress, so the line under it
/// reads in the unit of his example).
pub(crate) fn upload_files(
    dir: &Path,
    upload: crate::app::FixtureUpload,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    use crate::app::FixtureUpload::{Album, File, Idle, Late};
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    match upload {
        Album => {
            for n in 0..7_u32 {
                let hue = (n as f32 * 0.137 + 0.02).fract();
                let (w, h) = (800_u32, 600_u32);
                let bytes = png(w, h, |x, y| {
                    let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
                    let disc =
                        ((fx - 0.3 - 0.06 * n as f32).powi(2) + (fy - 0.45).powi(2)).sqrt() < 0.16;
                    hsl(
                        hue,
                        0.55,
                        if disc {
                            0.86
                        } else {
                            0.30 + 0.28 * (fx + fy) / 2.0
                        },
                    )
                });
                let path = dir.join(format!("IMG_{:04}.png", 4100 + n));
                std::fs::write(&path, bytes)?;
                out.push(path);
            }
        }
        File | Late => out.push(upload_document(dir, "Trip report.pdf", 30 << 20)?),
        Idle => {}
    }
    Ok(out)
}

/// A file of `len` bytes of a fixed pseudo-random sequence (a document the user picked).
pub(crate) fn upload_document(
    dir: &Path,
    name: &str,
    len: usize,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    let path = dir.join(name);
    std::fs::write(&path, bytes)?;
    Ok(path)
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
    let Some((peer, last)) = lowest_plain_chat(&script, &[calls_peer]) else {
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
        // (The sender says 200 x 90, the file is 90 x 200, the way a phone that ignores the
        // EXIF rotation says it: the download changes the row's height, the case of the held
        // row in `retain_ui`.)
        media(vec![synthetic_image_claiming((200, 90), 90, 200, 61)], None),
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

/// How long the temporary ban of `--page=waban` has left, from the fixture's "now".
const BAN_LEFT_MS: i64 = 23 * 3_600_000;

/// How long the reach-out lock of `--page=walock` has left, from the fixture's "now".
const LOCK_LEFT_MS: i64 = 3 * 24 * 3_600_000;

/// What a page of WhatsApp's own states adds to the fixture account (K-A; `FixtureWhatsApp`).
/// The account is played by the fake as WhatsApp; no chat is added, the rows go into the chat
/// [`lowest_plain_chat`] finds (the pages that open it are told by `FixtureInfo`).
///
/// - `Banned`: the connection ends `Restricted` for 23 more hours (a 101), with WhatsApp's own
///   words and a page to appeal on (an address that cannot be reached: `.invalid`);
/// - `Locked`: the backend states the reach-out lock, three days from "now", at the end; the
///   chat gets an own message that waits for it (the app sends it once the hold stands);
/// - `Filling`: a batch of older rows of the chat arrives a second after the start (and again
///   every second for half a minute), says more follows and is at 40 %. Returns the number of
///   steps a start waits for (everything before the first batch);
/// - `Refused`: an own message, imported as refused for being a first contact, is the chat's
///   newest row (dated minutes after its newest message, days before the next chat);
/// - `Lost`: the notice that two chunks of the pushed history never arrived comes a second after
///   the start (and again every second for half a minute: the viewmodel keeps one line for an
///   account, so repeating it is harmless). No chat is touched; the banner is the page.
///
/// Returns the script, the chat's peer when the page uses one, and the steps a start waits
/// for when more follow.
pub(crate) fn with_whatsapp_page(
    script: Script,
    page: crate::app::FixtureWhatsApp,
    now_ms: i64,
    taken: &[Option<&UserId>],
) -> (Script, Option<UserId>, Option<usize>) {
    use crate::app::FixtureWhatsApp::{Banned, Filling, Locked, Lost, Refused};
    use kourier_model::{
        AccountHold, BanReason, ConnState, FailReason, HoldReason, NetworkNote, OutgoingState,
        Restriction,
    };
    let ms = |left: i64| u64::try_from(now_ms.saturating_add(left)).unwrap_or(0);
    match page {
        Banned => {
            let state = ConnState::Restricted {
                until_ms: Some(ms(BAN_LEFT_MS)),
                reason: "fixture: temporary ban".into(),
                kind: Restriction::Banned(BanReason::TooManyStrangers),
                note: NetworkNote {
                    explanation: Some(
                        "Your account is temporarily banned. You can use WhatsApp again when \
                         the time is up."
                            .into(),
                    ),
                    appeal_url: Some("https://support.invalid/whatsapp/appeal".into()),
                },
            };
            (script.step(Step::Conn(state)), None, None)
        }
        Locked => {
            let hold = AccountHold {
                until_ms: Some(ms(LOCK_LEFT_MS)),
                reason: HoldReason::ReachOutLocked,
            };
            let event = BackendEvent::AccountHold(Some(hold));
            // The chat of the message the page queues (`FixtureOptions::whatsapp`, seeded by the
            // app once the hold stands): the wait line under it is what the page pictures.
            let peer = lowest_plain_chat(&script, taken).map(|(peer, _)| peer);
            (script.step(Step::Ephemeral(Box::new(event))), peer, None)
        }
        Lost => {
            let notice = BackendEvent::Notice(BackendNotice::HistoryLost { count: 2 });
            // (A start waits for what is before the first notice; the notice is live and
            // repeats, so a window that is slow to start sees one.)
            let mut script = script.step(Step::WaitAcked);
            let seed_steps = script.steps.len();
            for _ in 0..30 {
                script = script
                    .step(Step::Sleep(std::time::Duration::from_secs(1)))
                    .deliver(vec![notice.clone()]);
            }
            (script, None, Some(seed_steps))
        }
        Filling | Refused => {
            let Some((peer, last)) = lowest_plain_chat(&script, taken) else {
                return (script, None, None);
            };
            let key = ConvKey::Direct(peer.clone());
            if page == Refused {
                let mut message = kourier_fake::scripts::own(
                    &key,
                    "wa-refused-1",
                    last + 5 * 60_000,
                    "Hello, it is the neighbour from the market. Are you free to talk tomorrow?",
                );
                message.initial_out_state = Some(OutgoingState::Failed {
                    reason: FailReason::ReachOutRefused,
                    retryable: false,
                });
                let batch = BackendEvent::HistoryBatch {
                    conv: key,
                    messages: vec![message],
                    has_more: false,
                    progress: None,
                };
                return (
                    deliver_before_final_state(script, vec![batch]),
                    Some(peer),
                    None,
                );
            }
            // Older rows of the chat: it keeps its newest message, so its place in the list.
            let messages = (1..=3_i64)
                .map(|n| {
                    let at = last - n * 3_600_000;
                    let mut m = kourier_fake::scripts::remote(
                        &key,
                        &peer,
                        &format!("wa-history-{n}"),
                        at,
                        "An older message the phone sent on.",
                    );
                    m.read = true;
                    m
                })
                .collect();
            let batch = BackendEvent::HistoryBatch {
                conv: key,
                messages,
                has_more: true,
                progress: Some(0.4),
            };
            // (A start waits for what is before the batch. The batch is live: it comes a second
            // later and again every second, as the reports of `FixtureOptions::updating` do, so a
            // window that is slow to start sees one. The core takes a row it has again as
            // nothing new.)
            let mut script = script.step(Step::WaitAcked);
            let seed_steps = script.steps.len();
            for _ in 0..30 {
                script = script
                    .step(Step::Sleep(std::time::Duration::from_secs(1)))
                    .deliver(vec![batch.clone()]);
            }
            (script, Some(peer), Some(seed_steps))
        }
    }
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
    fn the_files_the_upload_pages_send_carry_no_location() {
        // The fixture pages send through the same `SendFiles` as the composer, without an
        // agreement for any file: a located file among them would be refused by the core
        // (the `Location` backstop) and the page would show no upload.
        use crate::app::FixtureUpload::{Album, File, Idle, Late};
        use kourier_core::facade::{Look, look_file};
        let dir = tempfile::tempdir().expect("dir");
        let mut looked = 0;
        for (n, upload) in [Album, File, Idle, Late].into_iter().enumerate() {
            let files = upload_files(&dir.path().join(n.to_string()), upload).expect("files");
            for path in files {
                let mut f = std::fs::File::open(&path).expect("open");
                let look = look_file(&mut f);
                assert!(!matches!(look, Look::Found(_)), "{look:?}");
                looked += 1;
            }
        }
        assert!(looked >= 8, "{looked}");
    }

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

    /// `FixtureOptions::person_groups` (the tests of "Show All"): she shares as many groups as
    /// asked, still without a chat added.
    #[test]
    fn the_person_can_share_more_groups_for_a_test() {
        use kourier_fake::fixture::{FixtureSpec, fixture};
        let script = fixture(&FixtureSpec {
            messages_per_conversation: 10,
            ..Default::default()
        })
        .expect("fixture");
        let count = |s: &Script, peer: &UserId| {
            s.deliveries()
                .into_iter()
                .flatten()
                .filter(|e| {
                    matches!(e, BackendEvent::ConversationUpsert(p)
                    if matches!(p.key, ConvKey::Group(_))
                        && p.members.as_ref().is_some_and(|l| l.upsert.iter().any(|m| m.user == *peer)))
                })
                .count()
        };
        let (many, peer) = with_person(script, &[], 7);
        let peer = peer.expect("a contact in groups");
        assert!(count(&many, &peer) >= 7, "{}", count(&many, &peer));
    }

    #[test]
    fn the_person_page_picks_the_contact_in_the_most_groups_and_adds_no_chat() {
        use kourier_fake::fixture::{FixtureSpec, fixture};
        let script = fixture(&FixtureSpec {
            messages_per_conversation: 10,
            ..Default::default()
        })
        .expect("fixture");
        let events =
            |s: &Script| -> Vec<BackendEvent> { s.deliveries().into_iter().flatten().collect() };
        let chats = |s: &Script| {
            events(s)
                .iter()
                .filter_map(|e| match e {
                    BackendEvent::ConversationUpsert(p) => Some(p.key.clone()),
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        // The members that were told for the group with the newest message (the `chat` page's).
        let newest_group = events(&script)
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if matches!(m.conv, ConvKey::Group(_)) => {
                    Some((m.sent_at, m.conv.clone()))
                }
                _ => None,
            })
            .max()
            .map(|(_, k)| k)
            .expect("a group with messages");
        let told = |s: &Script| {
            events(s)
                .iter()
                .filter(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == newest_group && p.members.is_some()))
                .count()
        };
        let before = chats(&script);
        let told_before = told(&script);
        let (script, peer) = with_person(script, &[], PERSON_GROUPS);
        let peer = peer.expect("a contact in groups");
        assert_eq!(
            chats(&script),
            before,
            "no chat is added: the list does not move"
        );
        assert_eq!(
            told(&script),
            told_before,
            "the newest group keeps its members: the chat page does not move"
        );
        // Her groups, against everybody else's.
        let mut groups: std::collections::HashMap<UserId, usize> = std::collections::HashMap::new();
        for e in events(&script) {
            if let BackendEvent::ConversationUpsert(p) = e
                && matches!(p.key, ConvKey::Group(_))
            {
                for m in p.members.iter().flat_map(|l| &l.upsert) {
                    *groups.entry(m.user.clone()).or_insert(0) += 1;
                }
            }
        }
        let hers = groups[&peer];
        assert!(hers >= 3, "the page needs a few groups to list: {hers}");
        // (Against the contacts with a chat that is not archived: that chat is what opens.)
        let archived: Vec<UserId> = events(&script)
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p) if p.archived == Some(true) => {
                    p.key.direct_peer().cloned()
                }
                _ => None,
            })
            .collect();
        let with_chat: Vec<UserId> = events(&script)
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p) => p.key.direct_peer().cloned(),
                _ => None,
            })
            .filter(|u| !archived.contains(u))
            .collect();
        assert!(
            with_chat
                .iter()
                .all(|u| groups.get(u).copied().unwrap_or(0) <= hers),
            "the contact in the most groups"
        );
        // The one update: an about text with its emoji and a number she shares.
        let patch = events(&script)
            .into_iter()
            .find_map(|e| match e {
                BackendEvent::ContactUpsert(p) if p.user == peer && p.number_shown.is_some() => {
                    Some(p)
                }
                _ => None,
            })
            .expect("her contact update");
        assert!(matches!(patch.about, Patch::Set(_)) && matches!(patch.about_emoji, Patch::Set(_)));
        assert_eq!(patch.number_shown, Some(true));
        // A peer another page uses is left alone.
        let again = fixture(&FixtureSpec {
            messages_per_conversation: 10,
            ..Default::default()
        })
        .expect("fixture");
        let busy: Vec<UserId> = groups
            .iter()
            .filter(|(_, n)| **n >= 3)
            .map(|(u, _)| u.clone())
            .collect();
        let taken: Vec<Option<&UserId>> = busy.iter().map(Some).collect();
        let (again, other) = with_person(again, &taken, PERSON_GROUPS);
        let other = other.expect("another contact");
        assert_ne!(other, peer);
        // Fewer generated groups are made up to three by member deltas, never in the newest group.
        let hers_now = events(&again)
            .iter()
            .filter(|e| {
                matches!(e, BackendEvent::ConversationUpsert(p)
                if matches!(p.key, ConvKey::Group(_))
                    && p.members.as_ref().is_some_and(|l| l.upsert.iter().any(|m| m.user == other)))
            })
            .count();
        assert!(hers_now >= 3, "{hers_now}");
        assert_eq!(
            told(&again),
            told_before,
            "the newest group keeps its members"
        );
    }

    #[test]
    fn the_person_page_never_changes_the_members_of_the_newest_group() {
        use kourier_fake::fixture::{FixtureSpec, fixture};
        // Three groups only: to have three, she would have to join the newest one.
        let script = fixture(&FixtureSpec {
            messages_per_conversation: 10,
            groups: 3,
            ..Default::default()
        })
        .expect("fixture");
        let events =
            |s: &Script| -> Vec<BackendEvent> { s.deliveries().into_iter().flatten().collect() };
        let newest_group = events(&script)
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if matches!(m.conv, ConvKey::Group(_)) => {
                    Some((m.sent_at, m.conv.clone()))
                }
                _ => None,
            })
            .max()
            .map(|(_, k)| k)
            .expect("a group with messages");
        let told = |s: &Script| {
            events(s)
                .iter()
                .filter(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == newest_group && p.members.is_some()))
                .count()
        };
        let before = told(&script);
        // Every contact in turn is the person (the ones before are taken), each topped up to
        // three groups where it can be.
        let mut taken: Vec<UserId> = Vec::new();
        loop {
            let refs: Vec<Option<&UserId>> = taken.iter().map(Some).collect();
            let (after, peer) = with_person(script.clone(), &refs, PERSON_GROUPS);
            let Some(peer) = peer else { break };
            assert_eq!(told(&after), before, "the newest group keeps its members");
            taken.push(peer);
        }
        assert!(taken.len() >= 3, "several contacts were tried");
    }

    /// The members of `key` after every delivery of the script, by their roles.
    fn members_of(
        script: &Script,
        key: &ConvKey,
    ) -> std::collections::BTreeMap<UserId, kourier_model::MemberRole> {
        let mut roles = std::collections::BTreeMap::new();
        for e in script.deliveries().into_iter().flatten() {
            if let BackendEvent::ConversationUpsert(p) = e
                && p.key == *key
                && let Some(list) = p.members
            {
                if list.full {
                    roles.clear();
                }
                for m in list.upsert {
                    roles.insert(m.user, m.role);
                }
                for gone in list.remove {
                    roles.remove(&gone);
                }
            }
        }
        roles
    }

    fn group_script() -> Script {
        kourier_fake::fixture::fixture(&kourier_fake::fixture::FixtureSpec {
            messages_per_conversation: 10,
            ..Default::default()
        })
        .expect("fixture")
    }

    /// `FixtureOptions::group_mentions` (people pages P3, LD): ten messages in the group of the page, the
    /// newest of it, from two named members; their mentions name members or the account, their ranges
    /// are valid, and one text carries a forged mention link. Without the option nothing is added.
    #[test]
    fn the_group_can_get_messages_that_mention_people_for_a_test() {
        use kourier_model::text::validate_range;
        use kourier_model::{Origin, RangeKind};
        let (script, found) = with_group(group_script(), &[], 12);
        let group = found.expect("a group for the page");
        assert!(group.named.len() >= 2, "two named members can write");
        let me = script.own_ids[0].clone();
        let newest = newest_message(&script).expect("messages");
        let in_group = |s: &Script| -> Vec<IncomingMessage> {
            s.deliveries()
                .into_iter()
                .flatten()
                .filter_map(|e| match e {
                    BackendEvent::Message(m) if m.conv == group.key => Some(m),
                    _ => None,
                })
                .collect()
        };
        let plain = in_group(&script);
        let grown = with_group_mentions(script.clone(), &group, group.named.first());
        let all = in_group(&grown);
        assert_eq!(all.len(), plain.len() + 10);
        let added: Vec<&IncomingMessage> = all.iter().filter(|m| m.sent_at > newest).collect();
        assert_eq!(added.len(), 10, "the ten are the newest of all");
        let authors: std::collections::BTreeSet<UserId> = added
            .iter()
            .filter_map(|m| match &m.origin {
                Origin::Remote { author, .. } => Some(author.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(authors.len(), 2, "two members write them");
        assert!(authors.iter().all(|a| group.named.contains(a)));
        let mut mentioned = Vec::new();
        for m in &added {
            let Body::Text(t) = &m.body else {
                panic!("a text");
            };
            assert!(t.body.starts_with("P3 "), "{}", t.body);
            for r in &t.ranges {
                assert!(validate_range(&t.body, r).is_ok(), "{:?}", r);
                if let RangeKind::Mention(u) = &r.kind {
                    mentioned.push(u.clone());
                }
            }
        }
        assert!(mentioned.contains(&me), "one mention is of the account");
        assert!(
            mentioned
                .iter()
                .all(|u| *u == me || group.named.contains(u)),
            "the others are of members"
        );
        let forged = added
            .iter()
            .filter_map(|m| match &m.body {
                Body::Text(t) => Some(t),
                _ => None,
            })
            .any(|t| {
                t.ranges
                    .iter()
                    .any(|r| matches!(&r.kind, RangeKind::Link(l) if l == "kourier:mention/0"))
            });
        assert!(forged, "a message carries the forged mention link");
        // The group is not asked for when the option is off: the script is the one without it.
        assert_eq!(in_group(&script).len(), plain.len());
    }

    /// `FixtureOptions::group_timers`: the page's group offers the timers and lets every member
    /// change them, as the LAST word told of it (the measurement changes the group's timer).
    #[test]
    fn the_group_can_offer_its_timers_for_a_test() {
        let (script, found) = with_group(group_script(), &[], 0);
        let key = found.expect("a group for the page").key;
        let last_features = |s: &Script| {
            s.deliveries()
                .into_iter()
                .flatten()
                .filter_map(|e| match e {
                    BackendEvent::ConversationUpsert(p) if p.key == key => p.features,
                    _ => None,
                })
                .next_back()
                .expect("features of the group")
        };
        let plain = last_features(&script);
        assert!(
            plain.disappearing.is_empty(),
            "the page's group offers none"
        );
        assert!(plain.group_info.is_some_and(|g| g.attributes_admin_only));
        let timed = last_features(&with_group_timers(script, &key));
        assert!(!timed.disappearing.is_empty(), "the timers are offered");
        assert_eq!(
            timed.group_info,
            Some(kourier_model::GroupInfo::default()),
            "and every member may change them"
        );
    }

    /// The group of `--page=group` (people pages P2): not the group with the newest message (the
    /// `chat` page's), with a description, the rules of a Signal group the account is no admin of
    /// and one invited member; no chat is added and no other group is touched.
    #[test]
    fn the_group_page_picks_a_group_that_is_not_the_newest_and_adds_no_chat() {
        let script = group_script();
        let events =
            |s: &Script| -> Vec<BackendEvent> { s.deliveries().into_iter().flatten().collect() };
        let chats = |s: &Script| {
            events(s)
                .iter()
                .filter_map(|e| match e {
                    BackendEvent::ConversationUpsert(p) => Some(p.key.clone()),
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        let newest_group = events(&script)
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if matches!(m.conv, ConvKey::Group(_)) => {
                    Some((m.sent_at, m.conv.clone()))
                }
                _ => None,
            })
            .max()
            .map(|(_, k)| k)
            .expect("a group with messages");
        // What is told of every group: the number of upserts, with their features and members.
        let told = |s: &Script, key: &ConvKey| {
            events(s)
                .iter()
                .filter(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == *key))
                .count()
        };
        let before = chats(&script);
        let newest_told = told(&script, &newest_group);
        let (grown, found) = with_group(script.clone(), &[], 0);
        let found = found.expect("a group for the page");
        assert_ne!(
            found.key, newest_group,
            "the chat page's group is left alone"
        );
        assert_eq!(
            chats(&grown),
            before,
            "no chat is added: the list does not move"
        );
        assert_eq!(
            told(&grown, &newest_group),
            newest_told,
            "the newest group is told nothing more: the chat page does not move"
        );
        // Only that group is told more, with ONE upsert.
        assert_eq!(told(&grown, &found.key), told(&script, &found.key) + 1);
        let patch = events(&grown)
            .into_iter()
            .find_map(|e| match e {
                BackendEvent::ConversationUpsert(p)
                    if p.key == found.key && matches!(p.description, Patch::Set(_)) =>
                {
                    Some(p)
                }
                _ => None,
            })
            .expect("the group's description");
        assert!(matches!(&patch.description, Patch::Set(d) if d == GROUP_DESCRIPTION));
        let features = patch.features.expect("the group's rules");
        assert!(
            features
                .group_info
                .is_some_and(|i| i.attributes_admin_only && !i.members_admin_only),
            "only admins change the group info"
        );
        assert!(
            features.disappearing.is_empty() && features.disappearing_custom.is_empty(),
            "Signal's group offers no timer: the page shows the read-only value and why"
        );
        // One invited member, a contact of the fixture.
        let list = patch.members.expect("the invited member");
        assert!(!list.full && list.remove.is_empty());
        assert_eq!(list.upsert.len(), 1);
        assert_eq!(list.upsert[0].role, kourier_model::MemberRole::Pending);
        let invited = list.upsert[0].user.clone();
        // A peer another page uses is never the invited one.
        let (other, _) = with_group(script.clone(), &[Some(&invited)], 0);
        let roles = members_of(&other, &found.key);
        assert_ne!(
            roles
                .iter()
                .find(|(_, r)| **r == kourier_model::MemberRole::Pending),
            Some((&invited, &kourier_model::MemberRole::Pending))
        );
    }

    /// The chat page's group (the newest message) is never the group page's, even when it is the
    /// one with the most members: the group page changes the group it shows.
    #[test]
    fn the_group_page_leaves_the_newest_group_alone_even_when_it_is_the_biggest() {
        use kourier_model::{Member, MemberList, MemberRole};
        let script = group_script();
        let events =
            |s: &Script| -> Vec<BackendEvent> { s.deliveries().into_iter().flatten().collect() };
        let newest_group = events(&script)
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) if matches!(m.conv, ConvKey::Group(_)) => {
                    Some((m.sent_at, m.conv.clone()))
                }
                _ => None,
            })
            .max()
            .map(|(_, k)| k)
            .expect("a group with messages");
        let crowd: Vec<Member> = (0..30)
            .filter_map(|n| {
                UserId::parse(&format!(
                    "aci:00000000-0000-4000-8000-{:012x}",
                    0x00ab_c000 + n
                ))
                .ok()
            })
            .map(|user| Member {
                user,
                role: MemberRole::Member,
            })
            .collect();
        assert_eq!(crowd.len(), 30);
        let mut patch = ConversationPatch::new(newest_group.clone());
        patch.members = Some(MemberList {
            full: false,
            upsert: crowd,
            remove: Vec::new(),
        });
        let script =
            deliver_before_final_state(script, vec![BackendEvent::ConversationUpsert(patch)]);
        let (_, found) = with_group(script, &[], 0);
        let found = found.expect("a group for the page");
        assert_ne!(
            found.key, newest_group,
            "the biggest group is the newest one: the page takes another"
        );
    }

    /// The members of the group page's group grow for a test (never on a fixture page): up to
    /// the number asked, with one made-up member whose name is markup (it has a contact row)
    /// and one without a contact row at all (the fallback name).
    #[test]
    fn the_group_can_grow_for_a_test_with_a_markup_name_and_a_member_without_a_contact() {
        use kourier_model::MemberRole;
        let script = group_script();
        let (grown, found) = with_group(script, &[], 12);
        let found = found.expect("a group for the page");
        let roles = members_of(&grown, &found.key);
        let full = roles
            .values()
            .filter(|r| matches!(r, MemberRole::Member | MemberRole::Admin))
            .count();
        assert!(full >= 12, "{full} full members");
        assert_eq!(
            roles
                .values()
                .filter(|r| **r == MemberRole::Pending)
                .count(),
            1,
            "the invited member stays"
        );
        let contacts: Vec<(UserId, Patch<String>)> = grown
            .deliveries()
            .into_iter()
            .flatten()
            .filter_map(|e| match e {
                BackendEvent::ContactUpsert(p) => Some((p.user, p.profile_name)),
                _ => None,
            })
            .collect();
        let markup = contacts
            .iter()
            .find(|(_, name)| matches!(name, Patch::Set(n) if n == GROUP_MARKUP_NAME))
            .map(|(u, _)| u.clone())
            .expect("a member whose name is markup");
        assert_eq!(roles.get(&markup), Some(&MemberRole::Member));
        let bare = roles
            .keys()
            .filter(|u| roles[*u] == MemberRole::Member && !contacts.iter().any(|(c, _)| c == *u));
        assert!(
            bare.count() >= 1,
            "a member without a contact row: its name is the fallback"
        );
        // Without the option nothing is grown.
        let (plain, found) = with_group(group_script(), &[], 0);
        let plain_full = members_of(&plain, &found.expect("a group").key)
            .values()
            .filter(|r| matches!(r, MemberRole::Member | MemberRole::Admin))
            .count();
        assert!(plain_full < 12, "{plain_full}");
    }

    #[test]
    fn a_grown_group_holds_the_people_other_pages_use_first() {
        use kourier_model::MemberRole;
        let (plain, found) = with_group(group_script(), &[], 0);
        let key = found.expect("a group").key;
        let before = members_of(&plain, &key);
        // The contact that sorts last and is in no role of the group: nothing else would reach
        // it with a small target.
        let mut contacts: Vec<UserId> = group_script()
            .deliveries()
            .into_iter()
            .flatten()
            .filter_map(|e| match e {
                BackendEvent::ContactUpsert(p) => Some(p.user),
                _ => None,
            })
            .filter(|u| !before.contains_key(u))
            .collect();
        contacts.sort();
        let peer = contacts
            .last()
            .cloned()
            .expect("a contact outside the group");
        let (grown, _) = with_group(group_script(), &[Some(&peer)], 6);
        assert_eq!(
            members_of(&grown, &key).get(&peer),
            Some(&MemberRole::Member),
            "the person another page uses is a member of the grown group"
        );
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
