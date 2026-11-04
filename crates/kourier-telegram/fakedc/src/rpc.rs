//! The API answers: `help.getConfig`, `auth.sendCode`, `auth.signIn`, `auth.logOut` (TD-4a A4),
//! `auth.bindTempAuthKey`, `auth.exportLoginToken` (QR: a token until the control socket's
//! `scan`, then the authorization), `updates.getState`, `updates.getDifference` and
//! `updates.getChannelDifference` (empty, difference, too long with the dialog) from the world's
//! pts (C4a), and from the world (C4b): `messages.getDialogs` (paged), `messages.getPinnedDialogs`
//! (folders 0 and 1: nothing pinned), `messages.getPeerDialogs`, `messages.getHistory`,
//! `messages.getMessages`, `channels.getMessages`, `channels.getChannels`,
//! `channels.getFullChannel`, `messages.getChats`, `messages.getFullChat`, `users.getUsers`,
//! `account.updateStatus`, `messages.getDialogFilters` (none), `messages.sendMessage` (a message
//! of the world: its pts follows the world's), `messages.readHistory` and `channels.readHistory`
//! (the account's own read, TD-4 K1: the world's read mark raised, see [`World::read_by_self`];
//! with `hold-reads on` neither applied nor answered, [`HELD`]). Every `phone.*` request is answered 400
//! `FAKE_PHONE_REFUSED` (and counted by the session: the 0002 proof); anything else
//! `rpc_error 400 FAKE_NOT_IMPLEMENTED`. Nothing of a request's fields is logged or printed.

use crate::tlb;
use crate::wire::{tl_bytes, u32_at};
use crate::world::{ChatId, HistoryQuery, World};
use grammers_tl_types as tl;
use std::net::SocketAddr;
use std::sync::Mutex;
use tl::{Deserializable, Identifiable, Serializable};

pub use crate::world::OWN_USER_ID;

/// The account's phone after a QR login (an original the scrub replaces; a code login names the
/// typed digits instead).
pub const QR_OWN_PHONE: &str = "999061000100";

pub const RPC_RESULT: u32 = 0xf35c6d01;
const RPC_ERROR: u32 = 0x2144ca19;
const BIND_TEMP_AUTH_KEY: u32 = 0xcdd42a05;
const BOOL_TRUE: u32 = 0x997275b5;
/// The QR token the fake DC hands out (a fixed test value; the scrub never sees it: TDLib shows
/// it only inside the `tg://login?token=` link of the login step, which no recording holds).
const LOGIN_TOKEN: &[u8] = b"fake-dc-login-token";

/// The "answer" of a request the fake DC holds (`hold-reads on`): no bytes at all, so the session
/// sends nothing back (MODEL: a request lost in flight). Never a valid TL answer.
pub const HELD: &[u8] = &[];

/// What one connection knows when it answers.
pub struct Ctx {
    pub now: i32,
    pub this_dc: i32,
    pub dialled: SocketAddr,
}

pub fn rpc_result(req_msg_id: i64, result: &[u8]) -> Vec<u8> {
    let mut v = RPC_RESULT.to_le_bytes().to_vec();
    v.extend(req_msg_id.to_le_bytes());
    v.extend(result);
    v
}

pub fn rpc_error(code: i32, word: &str) -> Vec<u8> {
    let mut v = RPC_ERROR.to_le_bytes().to_vec();
    v.extend(code.to_le_bytes());
    v.extend(tl_bytes(word.as_bytes()));
    v
}

/// The digits of a phone number as TDLib sends it (it may carry `+` and separators).
fn digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

/// The answer to one query (after its wrappers are peeled), from and to the shared `world`.
pub fn answer(ctx: &Ctx, world: &Mutex<World>, query: &[u8]) -> Vec<u8> {
    let Ok(mut w) = world.lock() else {
        return rpc_error(500, "FAKE_WORLD_POISONED");
    };
    w.date = w.date.max(ctx.now);
    answer_in(ctx, &mut w, query)
}

fn answer_in(ctx: &Ctx, w: &mut World, query: &[u8]) -> Vec<u8> {
    let Some(c) = u32_at(query, 0) else {
        return rpc_error(400, "FAKE_BAD_QUERY");
    };
    let body = &query[4..];
    let t = ctx.now;
    match c {
        tl::functions::help::GetConfig::CONSTRUCTOR_ID => {
            tlb::config(t, ctx.this_dc, ctx.dialled).to_bytes()
        }
        tl::functions::auth::SendCode::CONSTRUCTOR_ID => {
            if tl::functions::auth::SendCode::from_bytes(body).is_err() {
                return rpc_error(400, "FAKE_BAD_QUERY");
            }
            tl::enums::auth::SentCode::Code(tl::types::auth::SentCode {
                r#type: tl::enums::auth::SentCodeType::App(tl::types::auth::SentCodeTypeApp {
                    length: 5,
                }),
                phone_code_hash: "fakehash".into(),
                next_type: None,
                timeout: None,
            })
            .to_bytes()
        }
        tl::functions::auth::SignIn::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::auth::SignIn::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            tl::enums::auth::Authorization::Authorization(tl::types::auth::Authorization {
                setup_password_required: false,
                otherwise_relogin_days: None,
                tmp_sessions: None,
                future_auth_token: None,
                user: {
                    w.set_own_phone(&digits(&r.phone_number));
                    w.user_tl(OWN_USER_ID).expect("the world holds its account")
                },
            })
            .to_bytes()
        }
        tl::functions::updates::GetState::CONSTRUCTOR_ID => w.state().to_bytes(),
        tl::functions::updates::GetDifference::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::updates::GetDifference::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            w.difference_tl(r.pts, r.pts_total_limit).to_bytes()
        }
        tl::functions::updates::GetChannelDifference::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::updates::GetChannelDifference::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let tl::enums::InputChannel::Channel(c) = r.channel else {
                return rpc_error(400, "CHANNEL_INVALID");
            };
            match w.channel_difference_tl(c.channel_id, r.pts, r.limit) {
                Ok(d) => d.to_bytes(),
                Err(word) => rpc_error(400, word),
            }
        }
        tl::functions::messages::SendMessage::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::SendMessage::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let Some(chat) = w.chat_of_input(&r.peer) else {
                return rpc_error(400, "PEER_ID_INVALID");
            };
            let l = w.new_message(chat, OWN_USER_ID, &r.message);
            let crate::world::Change::New(_, m) = &l.change else {
                return rpc_error(500, "FAKE_WORLD");
            };
            if matches!(chat, ChatId::Channel(_)) {
                let id = tl::enums::Update::MessageId(tl::types::UpdateMessageId {
                    id: m.id,
                    random_id: r.random_id,
                });
                let mut u = crate::push::of_changes(w, std::slice::from_ref(&l), Vec::new());
                if let tl::enums::Updates::Updates(x) = &mut u {
                    x.updates.insert(0, id);
                }
                u.to_bytes()
            } else {
                tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
                    out: true,
                    id: m.id,
                    pts: l.pts,
                    pts_count: l.pts_count,
                    date: m.date,
                    media: None,
                    entities: None,
                    ttl_period: None,
                })
                .to_bytes()
            }
        }
        tl::functions::messages::ReadHistory::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::ReadHistory::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let Some(chat @ (ChatId::User(_) | ChatId::Basic(_))) = w.chat_of_input(&r.peer) else {
                return rpc_error(400, "PEER_ID_INVALID");
            };
            if w.hold_reads {
                return HELD.to_vec();
            }
            let Some(l) = w.read_by_self(chat, r.max_id) else {
                return rpc_error(500, "FAKE_WORLD");
            };
            tl::enums::messages::AffectedMessages::Messages(tl::types::messages::AffectedMessages {
                pts: l.pts,
                pts_count: l.pts_count,
            })
            .to_bytes()
        }
        tl::functions::channels::ReadHistory::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::channels::ReadHistory::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let tl::enums::InputChannel::Channel(c) = r.channel else {
                return rpc_error(400, "CHANNEL_INVALID");
            };
            if !w.chats.contains_key(&ChatId::Channel(c.channel_id)) {
                return rpc_error(400, "CHANNEL_INVALID");
            }
            if w.hold_reads {
                return HELD.to_vec();
            }
            if w.read_by_self(ChatId::Channel(c.channel_id), r.max_id)
                .is_none()
            {
                return rpc_error(500, "FAKE_WORLD");
            }
            BOOL_TRUE.to_le_bytes().to_vec()
        }
        tl::functions::messages::GetDialogs::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::GetDialogs::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let folder = r.folder_id.unwrap_or(0);
            let offset_peer = w.chat_of_input(&r.offset_peer);
            w.dialogs_tl(folder, r.offset_date, offset_peer, r.limit)
                .to_bytes()
        }
        tl::functions::messages::GetPinnedDialogs::CONSTRUCTOR_ID => {
            w.pinned_dialogs_tl().to_bytes()
        }
        tl::functions::messages::GetPeerDialogs::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::GetPeerDialogs::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            w.peer_dialogs_tl(&r.peers).to_bytes()
        }
        tl::functions::messages::GetHistory::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::GetHistory::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let Some(chat) = w.chat_of_input(&r.peer) else {
                return rpc_error(400, "PEER_ID_INVALID");
            };
            let q = HistoryQuery {
                offset_id: r.offset_id,
                add_offset: r.add_offset,
                limit: r.limit,
                max_id: r.max_id,
                min_id: r.min_id,
            };
            w.history_tl(chat, q).to_bytes()
        }
        tl::functions::messages::GetMessages::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::GetMessages::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            w.messages_by_id_tl(&r.id).to_bytes()
        }
        tl::functions::channels::GetMessages::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::channels::GetMessages::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let tl::enums::InputChannel::Channel(c) = r.channel else {
                return rpc_error(400, "CHANNEL_INVALID");
            };
            match w.channel_messages_tl(c.channel_id, &r.id) {
                Ok(m) => m.to_bytes(),
                Err(word) => rpc_error(400, word),
            }
        }
        tl::functions::channels::GetChannels::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::channels::GetChannels::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let ids = r.id.iter().filter_map(|c| match c {
                tl::enums::InputChannel::Channel(c) => Some(ChatId::Channel(c.channel_id)),
                _ => None,
            });
            w.chats_tl(ids).to_bytes()
        }
        tl::functions::channels::GetFullChannel::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::channels::GetFullChannel::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let tl::enums::InputChannel::Channel(c) = r.channel else {
                return rpc_error(400, "CHANNEL_INVALID");
            };
            match w.full_channel_tl(c.channel_id) {
                Ok(f) => f.to_bytes(),
                Err(word) => rpc_error(400, word),
            }
        }
        tl::functions::messages::GetChats::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::GetChats::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            w.chats_tl(r.id.iter().map(|c| ChatId::Basic(*c)))
                .to_bytes()
        }
        tl::functions::messages::GetFullChat::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::messages::GetFullChat::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            match w.full_chat_tl(r.chat_id) {
                Ok(f) => f.to_bytes(),
                Err(word) => rpc_error(400, word),
            }
        }
        tl::functions::users::GetUsers::CONSTRUCTOR_ID => {
            let Ok(r) = tl::functions::users::GetUsers::from_bytes(body) else {
                return rpc_error(400, "FAKE_BAD_QUERY");
            };
            let users: Vec<tl::enums::User> =
                r.id.iter()
                    .filter_map(|u| match u {
                        tl::enums::InputUser::User(u) => w.user_tl(u.user_id),
                        tl::enums::InputUser::FromMessage(u) => w.user_tl(u.user_id),
                        tl::enums::InputUser::UserSelf => w.user_tl(OWN_USER_ID),
                        tl::enums::InputUser::Empty => None,
                    })
                    .collect();
            users.to_bytes()
        }
        tl::functions::account::UpdateStatus::CONSTRUCTOR_ID => BOOL_TRUE.to_le_bytes().to_vec(),
        tl::functions::messages::GetDialogFilters::CONSTRUCTOR_ID => {
            tl::enums::messages::DialogFilters::Filters(tl::types::messages::DialogFilters {
                tags_enabled: false,
                filters: Vec::new(),
            })
            .to_bytes()
        }
        tl::functions::auth::ExportLoginToken::CONSTRUCTOR_ID => {
            if w.qr_scanned {
                if w.own_phone().is_empty() {
                    // A QR login names no number: the account's own original.
                    w.set_own_phone(QR_OWN_PHONE);
                }
                tl::enums::auth::LoginToken::Success(tl::types::auth::LoginTokenSuccess {
                    authorization: tl::enums::auth::Authorization::Authorization(
                        tl::types::auth::Authorization {
                            setup_password_required: false,
                            otherwise_relogin_days: None,
                            tmp_sessions: None,
                            future_auth_token: None,
                            user: w.user_tl(OWN_USER_ID).expect("the world holds its account"),
                        },
                    ),
                })
                .to_bytes()
            } else {
                tl::enums::auth::LoginToken::Token(tl::types::auth::LoginToken {
                    expires: t + 30,
                    token: LOGIN_TOKEN.to_vec(),
                })
                .to_bytes()
            }
        }
        tl::functions::auth::LogOut::CONSTRUCTOR_ID => {
            tl::enums::auth::LoggedOut::Out(tl::types::auth::LoggedOut {
                future_auth_token: None,
            })
            .to_bytes()
        }
        BIND_TEMP_AUTH_KEY => BOOL_TRUE.to_le_bytes().to_vec(),
        c if grammers_tl_types::name_for_id(c).starts_with("phone.") => {
            rpc_error(400, "FAKE_PHONE_REFUSED")
        }
        _ => rpc_error(400, "FAKE_NOT_IMPLEMENTED"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> Mutex<World> {
        Mutex::new(World::new(1))
    }

    #[test]
    fn the_config_names_the_dialled_address_and_no_other_host() {
        let ctx = Ctx {
            now: 1_700_000_000,
            this_dc: 2,
            dialled: "[2001:db8::5]:443".parse().unwrap(),
        };
        let bytes = answer(
            &ctx,
            &world(),
            &tl::functions::help::GetConfig {}.to_bytes(),
        );
        let tl::enums::Config::Config(c) = tl::enums::Config::from_bytes(&bytes).unwrap();
        assert_eq!(c.this_dc, 2);
        let [tl::enums::DcOption::Option(o)] = c.dc_options.as_slice() else {
            panic!("one dc option");
        };
        assert!(o.ipv6 && o.id == 2 && o.port == 443 && o.ip_address == "2001:db8::5");
        assert_eq!(c.dc_txt_domain_name, "example.invalid");
        assert!(c.me_url_prefix.contains("example.invalid"));
    }

    #[test]
    fn a_logout_is_answered_logged_out() {
        let ctx = Ctx {
            now: 1,
            this_dc: 1,
            dialled: "192.0.2.1:443".parse().unwrap(),
        };
        let bytes = answer(&ctx, &world(), &tl::functions::auth::LogOut {}.to_bytes());
        let tl::enums::auth::LoggedOut::Out(l) =
            tl::enums::auth::LoggedOut::from_bytes(&bytes).unwrap();
        assert!(l.future_auth_token.is_none());
    }

    #[test]
    fn an_unknown_query_is_an_rpc_error() {
        let ctx = Ctx {
            now: 1,
            this_dc: 1,
            dialled: "192.0.2.1:443".parse().unwrap(),
        };
        let r = answer(&ctx, &world(), &0x1234_5678u32.to_le_bytes());
        assert_eq!(u32_at(&r, 0), Some(RPC_ERROR));
    }

    #[test]
    fn state_and_differences_come_from_the_world() {
        let ctx = Ctx {
            now: 50,
            this_dc: 1,
            dialled: "192.0.2.1:443".parse().unwrap(),
        };
        let w = Mutex::new(World::standard(1));
        let pts = w.lock().unwrap().pts();
        let bytes = answer(&ctx, &w, &tl::functions::updates::GetState {}.to_bytes());
        let tl::enums::updates::State::State(s) =
            tl::enums::updates::State::from_bytes(&bytes).unwrap();
        assert_eq!((s.pts, s.date), (pts, 50));
        let q = tl::functions::updates::GetDifference {
            pts: pts - 2,
            pts_limit: None,
            pts_total_limit: Some(1),
            date: 1,
            qts: 0,
            qts_limit: None,
        };
        let bytes = answer(&ctx, &w, &q.to_bytes());
        let tl::enums::updates::Difference::Slice(d) =
            tl::enums::updates::Difference::from_bytes(&bytes).unwrap()
        else {
            panic!("a slice");
        };
        let tl::enums::updates::State::State(st) = d.intermediate_state;
        assert_eq!(st.pts, pts - 1);
        let q = tl::functions::updates::GetChannelDifference {
            force: false,
            channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id: 401,
                access_hash: tlb::CHANNEL_ACCESS_HASH,
            }),
            filter: tl::enums::ChannelMessagesFilter::Empty,
            pts: 1,
            limit: 100,
        };
        let bytes = answer(&ctx, &w, &q.to_bytes());
        let tl::enums::updates::ChannelDifference::Difference(d) =
            tl::enums::updates::ChannelDifference::from_bytes(&bytes).unwrap()
        else {
            panic!("a channel difference");
        };
        assert!(d.r#final && d.new_messages.len() == 1);
    }
    fn test_ctx() -> Ctx {
        Ctx {
            now: 50,
            this_dc: 1,
            dialled: "192.0.2.1:443".parse().unwrap(),
        }
    }

    fn read_user(user_id: i64, max_id: i32) -> Vec<u8> {
        tl::functions::messages::ReadHistory {
            peer: tl::enums::InputPeer::User(tl::types::InputPeerUser {
                user_id,
                access_hash: 0,
            }),
            max_id,
        }
        .to_bytes()
    }

    fn read_group(chat_id: i64, max_id: i32) -> Vec<u8> {
        tl::functions::messages::ReadHistory {
            peer: tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id }),
            max_id,
        }
        .to_bytes()
    }

    fn read_channel(channel_id: i64, max_id: i32) -> Vec<u8> {
        tl::functions::channels::ReadHistory {
            channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                channel_id,
                access_hash: tlb::CHANNEL_ACCESS_HASH,
            }),
            max_id,
        }
        .to_bytes()
    }

    /// The `(pts, pts_count)` of a `messages.affectedMessages` answer.
    fn affected(bytes: &[u8]) -> (i32, i32) {
        let tl::enums::messages::AffectedMessages::Messages(a) =
            tl::enums::messages::AffectedMessages::from_bytes(bytes).unwrap();
        (a.pts, a.pts_count)
    }

    fn marks(w: &Mutex<World>, chat: ChatId) -> (i32, i32) {
        w.lock().unwrap().read_state(chat).unwrap()
    }

    #[test]
    fn a_read_history_raises_the_read_state_and_the_pts() {
        let ctx = test_ctx();
        let w = Mutex::new(World::standard(1));
        let (a, g) = (ChatId::User(201), ChatId::Basic(301));
        let n = w.lock().unwrap().pts();
        assert_eq!(
            marks(&w, a),
            (1, 0),
            "the account's own message read its chat"
        );
        // A read that raises the mark: pts + 1, and the mark is EXACTLY the requested id.
        assert_eq!(affected(&answer(&ctx, &w, &read_user(201, 3))), (n + 1, 1));
        assert_eq!(marks(&w, a), (3, 0));
        assert_eq!(affected(&answer(&ctx, &w, &read_user(201, 4))), (n + 2, 1));
        assert_eq!(marks(&w, a), (4, 0));
        {
            // The change is stored with its pts (a later difference carries it), not pushed.
            let wl = w.lock().unwrap();
            let crate::world::Diff::Changes { entries, .. } = wl.difference(n, None) else {
                panic!("changes");
            };
            assert!(matches!(
                entries.iter().map(|l| &l.change).collect::<Vec<_>>()[..],
                [
                    crate::world::Change::ReadInbox {
                        max_id: 3,
                        still_unread: 1,
                        ..
                    },
                    crate::world::Change::ReadInbox {
                        max_id: 4,
                        still_unread: 0,
                        ..
                    },
                ]
            ));
        }
        // A read at or below the mark changes nothing and moves no pts.
        assert_eq!(affected(&answer(&ctx, &w, &read_user(201, 4))), (n + 2, 0));
        assert_eq!(affected(&answer(&ctx, &w, &read_user(201, 2))), (n + 2, 0));
        assert_eq!(marks(&w, a), (4, 0), "a lower read never lowers the mark");
        assert_eq!(w.lock().unwrap().pts(), n + 2);
        // A basic group is the common box too.
        let id = {
            let mut wl = w.lock().unwrap();
            let l = wl.new_message(g, 201, "x");
            let crate::world::Change::New(_, m) = l.change else {
                panic!("new");
            };
            m.id
        };
        let before = w.lock().unwrap().pts();
        assert_eq!(
            affected(&answer(&ctx, &w, &read_group(301, id))),
            (before + 1, 1)
        );
        assert_eq!(marks(&w, g), (id, 0));
        // An unknown peer and a channel peer on the private-chat request are refused.
        for q in [read_user(999, 1), read_group(999, 1)] {
            assert_eq!(u32_at(&answer(&ctx, &w, &q), 0), Some(RPC_ERROR));
        }
        let q = tl::functions::messages::ReadHistory {
            peer: tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
                channel_id: 401,
                access_hash: tlb::CHANNEL_ACCESS_HASH,
            }),
            max_id: 1,
        };
        assert_eq!(u32_at(&answer(&ctx, &w, &q.to_bytes()), 0), Some(RPC_ERROR));
        assert_eq!(marks(&w, ChatId::Channel(401)), (0, 0));
    }

    #[test]
    fn a_channel_read_history_raises_the_channel_read_state() {
        let ctx = test_ctx();
        let w = Mutex::new(World::standard(1));
        let s = ChatId::Channel(401);
        let (common, channel) = {
            let wl = w.lock().unwrap();
            (wl.pts(), wl.channel_pts(401).unwrap())
        };
        assert_eq!(marks(&w, s), (0, 0));
        let bytes = answer(&ctx, &w, &read_channel(401, 1));
        assert_eq!(bytes, BOOL_TRUE.to_le_bytes().to_vec());
        assert_eq!(marks(&w, s), (1, 0));
        {
            // A channel read moves no pts and is stored nowhere: neither box changed.
            let wl = w.lock().unwrap();
            assert_eq!((wl.pts(), wl.channel_pts(401)), (common, Some(channel)));
            assert_eq!(wl.chats[&s].unread_count(), 0);
            assert_eq!(
                wl.channel_difference(401, channel, 10),
                crate::world::ChannelDiff::Empty { pts: channel }
            );
        }
        // The mark is exactly the requested id and is never lowered.
        answer(&ctx, &w, &read_channel(401, 7));
        assert_eq!(marks(&w, s), (7, 0));
        answer(&ctx, &w, &read_channel(401, 3));
        assert_eq!(marks(&w, s), (7, 0));
        // A channel the world lacks is refused.
        assert_eq!(
            u32_at(&answer(&ctx, &w, &read_channel(499, 1)), 0),
            Some(RPC_ERROR)
        );
    }

    #[test]
    fn held_reads_are_neither_answered_nor_applied() {
        let ctx = test_ctx();
        let w = Mutex::new(World::standard(1));
        let (a, s) = (ChatId::User(201), ChatId::Channel(401));
        let n = w.lock().unwrap().pts();
        w.lock().unwrap().hold_reads = true;
        assert_eq!(answer(&ctx, &w, &read_user(201, 3)), HELD);
        assert_eq!(answer(&ctx, &w, &read_channel(401, 1)), HELD);
        assert_eq!((marks(&w, a), marks(&w, s)), ((1, 0), (0, 0)));
        assert_eq!(w.lock().unwrap().pts(), n);
        // `off` drops what was held for good: nothing is applied later.
        w.lock().unwrap().hold_reads = false;
        assert_eq!((marks(&w, a), marks(&w, s)), ((1, 0), (0, 0)));
        assert_eq!(w.lock().unwrap().pts(), n);
        // An ordinary read applies again.
        assert_eq!(affected(&answer(&ctx, &w, &read_user(201, 3))), (n + 1, 1));
        assert_eq!(marks(&w, a), (3, 0));
    }
}
