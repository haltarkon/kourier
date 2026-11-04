//! The control socket (TD-4a C4b, brief §3.2): a Unix socket in the test's temporary directory,
//! one command per line, answered `ok <json counts>` or `err <word>`:
//!
//! - `world <alone|standard>`: the served world (the account's phone is kept);
//! - `push <step>` / `queue <step>`: a change of the world, pushed to TDLib's updates connection
//!   or only stored (the next difference carries it);
//! - `drop`: every client connection closed (TDLib reconnects with the same auth key);
//! - `scan`: the QR login token was scanned (`updateLoginToken`; the next export succeeds);
//! - `stats`: the counts; `mark <label>`: a marker line in the wire log;
//! - `read-state <chat>`: `ok {"inbox": n, "outbox": n}`, the chat's read marks (numbers only);
//! - `hold-reads <on|off>`: while on, a `messages.readHistory`/`channels.readHistory` request is
//!   neither applied nor answered (MODEL: a request lost in flight); `off` never applies the
//!   held ones later.
//!
//! Steps (chats as `u201`, `g301`, `c401`): `new <chat>` (an incoming text; the long form with
//! `long-new <chat>`), `edit <chat> <id>`, `delete <chat> <id>[,<id>...]`, `read-inbox <chat>
//! <max_id>`, `read-outbox <chat> <max_id>`, `too-long <channel>` (`updateChannelTooLong`),
//! `threshold <channel|common> <pts>` (the channel's, or the common box's, too-long threshold;
//! nothing is pushed), `gap <chat>`
//! (two new messages stored, a third pushed: a pts gap of two), `gap-fill <chat>` (three new
//! messages pushed in the order third, first, second: the gap filled right after), `call <user>`, `call-missed
//! <user>`, `upgrade <group> <new channel>`. Every text the world makes is an original the scrub
//! replaces (`world.json` is rewritten after every command).

use crate::push::{self, Form};
use crate::session::Shared;
use crate::world::{ChatId, Logged, World};
use grammers_tl_types::Serializable;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// The longest command line.
const MAX_LINE: usize = 256;

/// Binds the control socket at `path` (inside a tokio runtime).
pub fn bind(path: &Path) -> Result<UnixListener, String> {
    UnixListener::bind(path).map_err(|e| format!("bind the control socket: {e}"))
}

/// Serves the control socket until the process ends.
pub async fn serve(listener: UnixListener, sh: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let sh = sh.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let answer = if line.len() > MAX_LINE {
                    "err too-long".to_string()
                } else {
                    command(&sh, line.trim()).await
                };
                if wr
                    .write_all(format!("{answer}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
}

/// What one step does to the world: the pushes it makes (none for a queued step).
enum Effect {
    Pushes(Vec<Vec<u8>>),
    /// QUEUE: the change is in the world; its `Updates` object is held for a session that comes
    /// back (H-M1) and any difference carries it too.
    Held(Vec<Vec<u8>>),
    Stored,
}

fn chat_arg(a: Option<&str>) -> Result<ChatId, &'static str> {
    a.and_then(ChatId::parse).ok_or("bad-chat")
}

fn int_arg(a: Option<&str>) -> Result<i32, &'static str> {
    a.and_then(|s| s.parse().ok()).ok_or("bad-number")
}

/// The sender of a pushed message in `chat` (the peer, a member, or nobody for a channel post).
fn sender(w: &World, chat: ChatId) -> i64 {
    match chat {
        ChatId::User(u) => u,
        ChatId::Basic(_) => 201,
        ChatId::Channel(c) => match w.chats.get(&chat).map(|x| &x.kind) {
            Some(crate::world::Kind::Broadcast { .. }) => 0,
            _ => 202 + (c % 2),
        },
    }
}

/// An original text for the n-th pushed message (the scrub replaces it).
fn pushed_text(w: &World) -> String {
    format!("pushed original {}", w.served_texts.len() + 1)
}

fn new_in(w: &mut World, chat: ChatId) -> Result<Logged, &'static str> {
    if !w.chats.contains_key(&chat) {
        return Err("no-chat");
    }
    let from = sender(w, chat);
    let text = pushed_text(w);
    Ok(w.new_message(chat, from, &text))
}

/// Applies `step` to the world; `push` false: nothing is pushed (QUEUE).
fn step(w: &mut World, text: &str, push: bool) -> Result<Effect, &'static str> {
    let mut a = text.split_whitespace();
    let verb = a.next().ok_or("no-step")?;
    let one = |w: &mut World, l: Logged, form: Form| -> Effect {
        let seq = w.seq;
        let bytes = push::of_change(w, &l, form).to_bytes();
        if push {
            Effect::Pushes(vec![bytes])
        } else if w.seq == seq {
            // A short form: no seq of its own, safe to send later.
            Effect::Held(vec![bytes])
        } else {
            // A container took the next seq: a queued change must not (a later push would then
            // jump over it), so it is only stored and a difference carries it.
            w.seq = seq;
            Effect::Stored
        }
    };
    let only_push = |e: Option<grammers_tl_types::enums::Updates>| -> Result<Effect, &'static str> {
        let u = e.ok_or("refused")?;
        Ok(if push {
            Effect::Pushes(vec![u.to_bytes()])
        } else {
            Effect::Stored
        })
    };
    match verb {
        "new" | "long-new" => {
            let chat = chat_arg(a.next())?;
            let l = new_in(w, chat)?;
            let form = if verb == "new" {
                Form::Short
            } else {
                Form::Long
            };
            Ok(one(w, l, form))
        }
        "edit" => {
            let chat = chat_arg(a.next())?;
            let id = int_arg(a.next())?;
            let text = pushed_text(w);
            let l = w.edit_message(chat, id, &text).ok_or("no-message")?;
            Ok(one(w, l, Form::Short))
        }
        "delete" => {
            let chat = chat_arg(a.next())?;
            let ids: Vec<i32> = a
                .next()
                .ok_or("bad-number")?
                .split(',')
                .map(|s| s.parse().map_err(|_| "bad-number"))
                .collect::<Result<_, _>>()?;
            let l = w.delete_messages(chat, &ids).ok_or("no-message")?;
            Ok(one(w, l, Form::Short))
        }
        "read-inbox" | "read-outbox" => {
            let chat = chat_arg(a.next())?;
            let max = int_arg(a.next())?;
            let l = if verb == "read-inbox" {
                w.read_inbox(chat, max)
            } else {
                w.read_outbox(chat, max)
            }
            .ok_or("no-chat")?;
            Ok(one(w, l, Form::Short))
        }
        "too-long" => {
            let ChatId::Channel(c) = chat_arg(a.next())? else {
                return Err("not-a-channel");
            };
            only_push(push::channel_too_long(w, c))
        }
        "threshold" => {
            let which = a.next();
            if which == Some("common") {
                w.too_long_after = int_arg(a.next())?;
                return Ok(Effect::Stored);
            }
            let ChatId::Channel(c) = chat_arg(which)? else {
                return Err("not-a-channel");
            };
            let n = int_arg(a.next())?;
            w.channel_pts(c).ok_or("no-chat")?;
            w.set_channel_too_long_after(c, n);
            Ok(Effect::Stored)
        }
        "gap" => {
            let chat = chat_arg(a.next())?;
            new_in(w, chat)?;
            new_in(w, chat)?;
            let l = new_in(w, chat)?;
            Ok(one(w, l, Form::Short))
        }
        "gap-fill" => {
            // The gapped message first, the two it skipped right after (a push that arrives out
            // of order): TDLib may hold the first in its pending-pts buffer until they come.
            let chat = chat_arg(a.next())?;
            let l1 = new_in(w, chat)?;
            let l2 = new_in(w, chat)?;
            let l3 = new_in(w, chat)?;
            let seq = w.seq;
            let bytes: Vec<Vec<u8>> = [l3, l1, l2]
                .iter()
                .map(|l| push::of_change(w, l, Form::Short).to_bytes())
                .collect();
            Ok(if push {
                Effect::Pushes(bytes)
            } else {
                w.seq = seq;
                Effect::Stored
            })
        }
        "call" | "call-missed" => {
            let ChatId::User(u) = chat_arg(a.next())? else {
                return Err("not-a-user");
            };
            if verb == "call" {
                only_push(push::call_requested(w, u))
            } else {
                only_push(push::call_missed(w, u))
            }
        }
        "upgrade" => {
            let (ChatId::Basic(g), ChatId::Channel(c)) = (chat_arg(a.next())?, chat_arg(a.next())?)
            else {
                return Err("bad-chat");
            };
            only_push(push::upgrade(w, g, c))
        }
        _ => Err("unknown-step"),
    }
}

/// `hold-reads <on|off>`.
fn hold_reads(w: &mut World, arg: &str) -> Result<(), &'static str> {
    w.hold_reads = match arg.trim() {
        "on" => true,
        "off" => false,
        _ => return Err("bad-switch"),
    };
    Ok(())
}

/// `read-state <chat>`: the answer's JSON, the two read marks and nothing else.
fn read_state_json(w: &World, arg: &str) -> Result<String, &'static str> {
    let chat = chat_arg(Some(arg.trim()))?;
    let (inbox, outbox) = w.read_state(chat).ok_or("no-chat")?;
    Ok(format!("{{\"inbox\":{inbox},\"outbox\":{outbox}}}"))
}

/// One command's answer.
/// `push-many <n> <step>`: the step pushed `n` times, in one command, so the world file is written
/// once (the perf cases' growth and burst; a push per command rewrites it every time).
async fn push_many(sh: &Shared, rest: &str) -> Result<(), &'static str> {
    let (n, what) = rest.split_once(' ').ok_or("no-step")?;
    let n: u32 = n.parse().map_err(|_| "bad-number")?;
    for _ in 0..n {
        let effect = sh.with_world(|w| {
            w.date = w.date.max(crate::now_secs() as i32);
            step(w, what, true)
        })?;
        let Effect::Pushes(pushes) = effect else {
            return Err("not-pushed");
        };
        for p in pushes {
            sh.push(&p).await;
        }
    }
    Ok(())
}

pub async fn command(sh: &Shared, line: &str) -> String {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    // An answer that carries its own JSON instead of the counts (`read-state`).
    let mut payload: Option<String> = None;
    let result: Result<(), &'static str> = match verb {
        "world" => sh.with_world(|w| {
            let date = crate::now_secs() as i32;
            let phone = w.own_phone().to_string();
            *w = World::named(rest.trim(), date, &phone).ok_or("unknown-world")?;
            Ok(())
        }),
        "push" | "queue" => {
            let effect = sh.with_world(|w| {
                w.date = w.date.max(crate::now_secs() as i32);
                step(w, rest, verb == "push")
            });
            match effect {
                Ok(Effect::Pushes(ps)) => {
                    for p in ps {
                        sh.push(&p).await;
                    }
                    Ok(())
                }
                Ok(Effect::Held(ps)) => {
                    for p in ps {
                        sh.hold(p);
                    }
                    sh.count_queued();
                    Ok(())
                }
                Ok(Effect::Stored) => {
                    sh.count_queued();
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "push-many" => push_many(sh, rest).await,
        "drop" => {
            sh.drop_all().await;
            Ok(())
        }
        "scan" => {
            let p = sh.with_world(|w| {
                w.qr_scanned = true;
                Ok(push::login_token(w).to_bytes())
            });
            match p {
                Ok(p) => {
                    sh.push(&p).await;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        "stats" => Ok(()),
        "hold-reads" => sh.with_world(|w| hold_reads(w, rest)),
        "read-state" => sh
            .with_world(|w| read_state_json(w, rest))
            .map(|j| payload = Some(j)),
        "mark" => {
            let label = rest.trim();
            if label.is_empty()
                || label.len() > 64
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                Err("bad-label")
            } else {
                sh.mark(label);
                Ok(())
            }
        }
        _ => Err("unknown-command"),
    };
    sh.write_world_json();
    match result {
        Ok(()) => format!("ok {}", payload.unwrap_or_else(|| sh.stats_json())),
        Err(e) => format!("err {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_change_the_world_and_queue_pushes_nothing() {
        let mut w = World::standard(1_000);
        let pts = w.pts();
        assert!(matches!(
            step(&mut w, "queue-ignored", false),
            Err("unknown-step")
        ));
        assert!(matches!(
            step(&mut w, "new u201", false),
            Ok(Effect::Held(_))
        ));
        assert_eq!(w.pts(), pts + 1);
        let Ok(Effect::Pushes(p)) = step(&mut w, "gap u202", true) else {
            panic!("one push");
        };
        assert_eq!(p.len(), 1);
        assert_eq!(w.pts(), pts + 4, "a gap stores two and pushes the third");
        assert!(matches!(step(&mut w, "new x1", true), Err("bad-chat")));
        assert!(matches!(
            step(&mut w, "edit u201 999", true),
            Err("no-message")
        ));
        assert!(matches!(
            step(&mut w, "threshold c401 2", false),
            Ok(Effect::Stored)
        ));
        assert!(matches!(
            step(&mut w, "call u202", true),
            Ok(Effect::Pushes(_))
        ));
        assert!(matches!(
            step(&mut w, "call-missed u202", true),
            Ok(Effect::Pushes(_))
        ));
        assert!(matches!(
            step(&mut w, "upgrade g301 c404", true),
            Ok(Effect::Pushes(_))
        ));
        assert!(
            w.served_texts
                .iter()
                .any(|t| t.starts_with("pushed original"))
        );
    }
    #[test]
    fn the_common_threshold_makes_a_too_long_difference() {
        let mut w = World::standard(1_000);
        let n = w.pts();
        assert!(matches!(
            step(&mut w, "threshold common 2", false),
            Ok(Effect::Stored)
        ));
        for _ in 0..3 {
            step(&mut w, "new u201", false).unwrap();
        }
        assert_eq!(w.difference(n, None), crate::world::Diff::TooLong);
        assert!(matches!(
            w.difference(n + 1, None),
            crate::world::Diff::Changes { .. }
        ));
        assert!(matches!(
            w.difference_tl(n, None),
            grammers_tl_types::enums::updates::Difference::TooLong(t) if t.pts == w.pts()
        ));
        // The common word needs a number; the other words stay the channel's.
        assert!(matches!(
            step(&mut w, "threshold common x", false),
            Err("bad-number")
        ));
        assert!(matches!(
            step(&mut w, "threshold u201 2", false),
            Err("not-a-channel")
        ));
        step(&mut w, "threshold common 1000", false).unwrap();
        assert!(matches!(
            w.difference(n, None),
            crate::world::Diff::Changes { .. }
        ));
    }

    #[test]
    fn hold_reads_and_read_state_name_their_arguments() {
        let mut w = World::standard(1_000);
        assert!(!w.hold_reads);
        hold_reads(&mut w, "on").unwrap();
        assert!(w.hold_reads);
        hold_reads(&mut w, " off ").unwrap();
        assert!(!w.hold_reads);
        assert_eq!(hold_reads(&mut w, "maybe"), Err("bad-switch"));
        assert_eq!(hold_reads(&mut w, ""), Err("bad-switch"));
        assert_eq!(
            read_state_json(&w, "u201").unwrap(),
            "{\"inbox\":1,\"outbox\":0}"
        );
        w.read_by_self(ChatId::Channel(401), 1).unwrap();
        assert_eq!(
            read_state_json(&w, "c401").unwrap(),
            "{\"inbox\":1,\"outbox\":0}"
        );
        w.read_outbox(ChatId::User(201), 4).unwrap();
        assert_eq!(
            read_state_json(&w, "u201").unwrap(),
            "{\"inbox\":1,\"outbox\":4}"
        );
        assert_eq!(read_state_json(&w, "u999"), Err("no-chat"));
        assert_eq!(read_state_json(&w, "x1"), Err("bad-chat"));
        assert_eq!(read_state_json(&w, ""), Err("bad-chat"));
    }
}
