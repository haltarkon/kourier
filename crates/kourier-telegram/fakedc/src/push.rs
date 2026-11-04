//! What the fake DC pushes (TD-4a C4b, brief §3.2, P-5): the `Updates` object of one world change.
//!
//! A new text message in a private chat or a basic group goes out in the SHORT form by default
//! (`updateShortMessage` / `updateShortChatMessage`, as the server commonly sends them: no users,
//! no chats); [`Form::Long`] is the one variant in the long form (`updates{updateNewMessage}`).
//! Everything else (channels, edits, deletes, reads, service messages, calls, the upgrade) goes
//! in an `updates` container with the users and chats it names and the next `seq`; the login
//! token's scan is an `updateShort`. Which shape a change takes is the fake DC's MODEL (P-5):
//! a recording's behaviour that depends on it is never a golden fact.

use crate::tlb;
use crate::world::{Change, ChatId, Logged, OWN_USER_ID, World};
use grammers_tl_types as tl;

/// The shape of a new message's push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// The short forms where they exist (the default).
    Short,
    /// The long form `updates{updateNewMessage}` (P-5's one variant).
    Long,
}

/// An `updates` container with the next `seq` of the world.
pub fn container(
    w: &mut World,
    updates: Vec<tl::enums::Update>,
    users: Vec<tl::enums::User>,
    chats: Vec<tl::enums::Chat>,
) -> tl::enums::Updates {
    w.seq += 1;
    tl::enums::Updates::Updates(tl::types::Updates {
        updates,
        users,
        chats,
        date: w.date,
        seq: w.seq,
    })
}

/// The push of one logged change.
pub fn of_change(w: &mut World, l: &Logged, form: Form) -> tl::enums::Updates {
    if form == Form::Short
        && let Change::New(chat, m) = &l.change
        && m.service.is_none()
    {
        let out = m.from == OWN_USER_ID;
        match *chat {
            ChatId::User(user_id) => {
                return tl::enums::Updates::UpdateShortMessage(tl::types::UpdateShortMessage {
                    out,
                    mentioned: false,
                    media_unread: false,
                    silent: false,
                    id: m.id,
                    user_id,
                    message: m.text.clone(),
                    pts: l.pts,
                    pts_count: l.pts_count,
                    date: m.date,
                    fwd_from: None,
                    via_bot_id: None,
                    reply_to: None,
                    entities: None,
                    ttl_period: None,
                });
            }
            ChatId::Basic(chat_id) => {
                return tl::enums::Updates::UpdateShortChatMessage(
                    tl::types::UpdateShortChatMessage {
                        out,
                        mentioned: false,
                        media_unread: false,
                        silent: false,
                        id: m.id,
                        from_id: m.from,
                        chat_id,
                        message: m.text.clone(),
                        pts: l.pts,
                        pts_count: l.pts_count,
                        date: m.date,
                        fwd_from: None,
                        via_bot_id: None,
                        reply_to: None,
                        entities: None,
                        ttl_period: None,
                    },
                );
            }
            ChatId::Channel(_) => {}
        }
    }
    of_changes(w, std::slice::from_ref(l), Vec::new())
}

/// The long forms of `entries` (then `extra`) in one container, with their peers.
pub fn of_changes(
    w: &mut World,
    entries: &[Logged],
    extra: Vec<tl::enums::Update>,
) -> tl::enums::Updates {
    let mut updates: Vec<tl::enums::Update> = entries.iter().map(|l| w.update_of(l)).collect();
    updates.extend(extra);
    let (chats, users) = w.peers_of(entries);
    container(w, updates, users, chats)
}

/// `updateChannelTooLong` for `channel` (with its pts) and the channel.
pub fn channel_too_long(w: &mut World, channel: i64) -> Option<tl::enums::Updates> {
    let pts = w.channel_pts(channel)?;
    let chats = w.chat_tl(ChatId::Channel(channel)).into_iter().collect();
    let u = tl::enums::Update::ChannelTooLong(tl::types::UpdateChannelTooLong {
        channel_id: channel,
        pts: Some(pts),
    });
    Some(container(w, vec![u], Vec::new(), chats))
}

/// `updatePhoneCall{phoneCallRequested}` from `caller` with the caller (a new call of the world).
pub fn call_requested(w: &mut World, caller: i64) -> Option<tl::enums::Updates> {
    let user = w.user_tl(caller)?;
    let me = w.user_tl(OWN_USER_ID)?;
    let id = w.call_requested(caller);
    let call = tlb::call_requested(
        id,
        crate::world::CALL_ACCESS_HASH,
        w.date,
        caller,
        OWN_USER_ID,
    );
    let u = tl::enums::Update::PhoneCall(tl::types::UpdatePhoneCall { phone_call: call });
    Some(container(w, vec![u], vec![user, me], Vec::new()))
}

/// The newest call of `caller` ends unanswered: `updatePhoneCall{phoneCallDiscarded, missed}`
/// then the call line (`messageService{messageActionPhoneCall}`) in the private chat, one container.
pub fn call_missed(w: &mut World, caller: i64) -> Option<tl::enums::Updates> {
    let call_id = w
        .calls
        .iter()
        .rev()
        .find(|(_, c)| **c == caller)
        .map(|(id, _)| *id)?;
    let line = w.call_missed(call_id)?;
    let discarded = tl::enums::Update::PhoneCall(tl::types::UpdatePhoneCall {
        phone_call: tlb::call_missed(call_id),
    });
    let mut updates = vec![discarded];
    updates.push(w.update_of(&line));
    let (chats, users) = w.peers_of(std::slice::from_ref(&line));
    Some(container(w, updates, users, chats))
}

/// The basic group `chat_id` becomes the supergroup `channel_id`: both service lines and
/// `updateChannel`, with both chats' new state.
pub fn upgrade(w: &mut World, chat_id: i64, channel_id: i64) -> Option<tl::enums::Updates> {
    let (first, second) = w.upgrade(chat_id, channel_id)?;
    let channel = tl::enums::Update::Channel(tl::types::UpdateChannel { channel_id });
    Some(of_changes(w, &[first, second], vec![channel]))
}

/// The QR login token was scanned: `updateShort{updateLoginToken}`.
pub fn login_token(w: &World) -> tl::enums::Updates {
    tl::enums::Updates::UpdateShort(tl::types::UpdateShort {
        update: tl::enums::Update::LoginToken,
        date: w.date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::{Deserializable, Serializable};

    fn round(u: &tl::enums::Updates) -> tl::enums::Updates {
        tl::enums::Updates::from_bytes(&u.to_bytes()).unwrap()
    }

    #[test]
    fn private_and_basic_group_messages_go_short_unless_long_is_asked() {
        let mut w = World::standard(1_000);
        let l = w.new_message(ChatId::User(201), 201, "pushed one");
        let tl::enums::Updates::UpdateShortMessage(s) = round(&of_change(&mut w, &l, Form::Short))
        else {
            panic!("the short form");
        };
        assert_eq!(
            (s.user_id, s.pts, s.pts_count, s.out),
            (201, l.pts, 1, false)
        );
        let seq = w.seq;
        let g = w.new_message(ChatId::Basic(301), 202, "pushed two");
        let tl::enums::Updates::UpdateShortChatMessage(s) = of_change(&mut w, &g, Form::Short)
        else {
            panic!("the short chat form");
        };
        assert_eq!((s.chat_id, s.from_id, s.pts), (301, 202, g.pts));
        assert_eq!(w.seq, seq, "a short form carries no seq");
        let tl::enums::Updates::Updates(u) = round(&of_change(&mut w, &g, Form::Long)) else {
            panic!("the long form");
        };
        assert_eq!(u.seq, seq + 1);
        assert!(matches!(
            u.updates.as_slice(),
            [tl::enums::Update::NewMessage(_)]
        ));
        assert!(!u.users.is_empty() && !u.chats.is_empty());
    }

    #[test]
    fn channel_messages_and_service_lines_go_in_a_container_with_the_next_seq() {
        let mut w = World::standard(1_000);
        let l = w.new_message(ChatId::Channel(401), 202, "pushed three");
        let tl::enums::Updates::Updates(u) = of_change(&mut w, &l, Form::Short) else {
            panic!("a container");
        };
        assert!(matches!(
            u.updates.as_slice(),
            [tl::enums::Update::NewChannelMessage(_)]
        ));
        assert_eq!(u.seq, 2);
        let tl::enums::Updates::Updates(c) = round(&call_requested(&mut w, 202).unwrap()) else {
            panic!("a container");
        };
        assert!(matches!(
            c.updates.as_slice(),
            [tl::enums::Update::PhoneCall(_)]
        ));
        let tl::enums::Updates::Updates(m) = round(&call_missed(&mut w, 202).unwrap()) else {
            panic!("a container");
        };
        let [
            tl::enums::Update::PhoneCall(_),
            tl::enums::Update::NewMessage(n),
        ] = m.updates.as_slice()
        else {
            panic!("the discard, then the call line");
        };
        assert!(matches!(&n.message, tl::enums::Message::Service(s)
            if matches!(s.action, tl::enums::MessageAction::PhoneCall(_))));
        assert_eq!(m.seq, 4);
        let tl::enums::Updates::Updates(t) = channel_too_long(&mut w, 401).unwrap() else {
            panic!("a container");
        };
        assert!(matches!(t.updates.as_slice(),
            [tl::enums::Update::ChannelTooLong(c)] if c.pts == w.channel_pts(401)));
    }

    #[test]
    fn the_upgrade_carries_both_lines_and_the_chats_new_state() {
        let mut w = World::standard(1_000);
        let tl::enums::Updates::Updates(u) = round(&upgrade(&mut w, 301, 404).unwrap()) else {
            panic!("a container");
        };
        let [
            tl::enums::Update::NewMessage(_),
            tl::enums::Update::NewChannelMessage(_),
            tl::enums::Update::Channel(c),
        ] = u.updates.as_slice()
        else {
            panic!("migrate-to, migrate-from, updateChannel");
        };
        assert_eq!(c.channel_id, 404);
        assert!(u.chats.iter().any(|c| matches!(c,
            tl::enums::Chat::Chat(b) if b.deactivated && b.migrated_to.is_some())));
        assert!(
            upgrade(&mut w, 301, 405).is_none(),
            "an upgraded group stays upgraded"
        );
        assert!(!w.dialog_order(0).contains(&ChatId::Basic(301)));
        assert!(w.dialog_order(0).contains(&ChatId::Channel(404)));
    }
}
