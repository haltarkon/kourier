//! Taking a reaction back (architecture §3.5.1; `BackendEvent::Reaction`): a removal WITH an emoji
//! takes that emoji of the sender away (Signal, Threema); a removal with NO emoji takes away every
//! reaction the sender has on the message (WhatsApp allows one per sender and message and takes it
//! back with an empty one). Through the real core: the ingest on a real `core.db` and the
//! timeline query, and the whole core on the fake network playing WhatsApp.

use crate::support;

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{TimelineAnchor, TimelineQuery, UiCommand};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::scripts::{T0, remote, signal_features, target};
use kourier_fake::{Flavor, Step};
use kourier_model::{
    ConvKey, ConversationId, ConversationPatch, MessageId, MsgIdScope, NetworkKind, UserId,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

const WA_PEER: &str = "lid:1111";
const WA_BOB: &str = "lid:2222";
const WA_ME: &str = "lid:9999";

fn wa_user(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn chat() -> ConvKey {
    ConvKey::Direct(wa_user(WA_PEER))
}

/// A core keyed per sender (WhatsApp) with one chat whose features are WhatsApp's (`per_user`
/// reactions per sender: `Some(1)`) or a set network's (`None`), and one message of the peer.
fn harness(per_user_one: bool) -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([8; 32]),
        NetworkKind::Fake,
        MsgIdScope::ConversationAuthor,
        &[wa_user(WA_ME)],
    )
    .expect("harness");
    h.set_now(T0);
    let mut features = signal_features(false);
    if !per_user_one {
        features.reactions_per_user = None;
    }
    let mut patch = ConversationPatch::new(chat());
    patch.features = Some(features);
    h.apply(vec![
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(remote(&chat(), &wa_user(WA_PEER), "M1", T0, "hello")),
    ])
    .expect("the chat");
    (dir, h)
}

fn reaction(by: &str, emoji: &str, remove: bool, at: i64) -> BackendEvent {
    BackendEvent::Reaction {
        target: target(Some(&chat()), Some(&wa_user(WA_PEER)), "M1"),
        by: Some(wa_user(by)),
        emoji: emoji.into(),
        remove,
        at,
    }
}

/// What the timeline shows under the message: (emoji, how many people), in the query's order.
fn shown(h: &IngestHarness) -> Vec<(String, usize)> {
    let conv: i64 = h
        .connection()
        .query_row("SELECT id FROM conversations", [], |r| r.get(0))
        .expect("the chat");
    h.timeline(&TimelineQuery {
        conv: ConversationId(conv),
        anchor: TimelineAnchor::Newest,
        limit: 50,
    })
    .expect("timeline")
    .rows
    .into_iter()
    .flat_map(|r| r.reactions)
    .map(|g| (g.emoji, g.users.len()))
    .collect()
}

/// The stored rows of one sender: (emoji, at, removed).
fn rows_of(h: &IngestHarness, user: &str) -> Vec<(String, i64, bool)> {
    h.connection()
        .prepare("SELECT emoji, at, removed FROM reactions WHERE user_id = ?1 ORDER BY emoji")
        .expect("prepare")
        .query_map([user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn count_reactions(h: &IngestHarness) -> i64 {
    h.connection()
        .query_row("SELECT count(*) FROM reactions", [], |r| r.get(0))
        .expect("count")
}

fn one(emoji: &str) -> Vec<(String, usize)> {
    vec![(emoji.to_owned(), 1)]
}

// ---------- the whole core on the fake playing WhatsApp ----------

/// The setup's message of alice, in her chat with us.
fn setup_message() -> String {
    (T0 - 60_000).to_string()
}

async fn reactions_shown(env: &Env, conv: ConversationId) -> Vec<(String, usize)> {
    env.timeline(conv)
        .await
        .iter()
        .flat_map(|r| r.reactions.iter().map(|g| (g.emoji.clone(), g.users.len())))
        .collect()
}

/// The need of WhatsApp's mapping (W3b): its taken-back reaction arrives as a removal with no
/// emoji. A WhatsApp-like account reacts, the reaction shows in the timeline query, then the
/// empty removal comes and it is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whatsapp_like_reaction_is_taken_back_by_a_removal_without_an_emoji() {
    let local = setup_message();
    let event = |emoji: &str, remove: bool, at: i64| BackendEvent::Reaction {
        target: target(Some(&d_alice()), Some(&alice()), &local),
        by: Some(alice()),
        emoji: emoji.into(),
        remove,
        at,
    };
    let s = script("reaction-removal")
        .deliver(vec![event("👍", false, T0 - 50_000)])
        .step(Step::Sleep(Duration::from_secs(60)))
        .deliver(vec![event("", true, T0 - 40_000)]);
    let mut env = Env::new(Flavor::WhatsAppLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let _account = add_linked(&mut env).await;
    let conv = {
        let db = env.core_db();
        eventually("the chat", WAIT, || {
            let db = db.clone();
            async move { conv_id(&db, &d_alice()).is_some() }
        })
        .await;
        conv_id(&db, &d_alice()).expect("the chat")
    };
    eventually("the reaction shows", WAIT, || {
        let env = &env;
        async move { reactions_shown(env, conv).await == one("👍") }
    })
    .await;
    // The removal waits behind the script's sleep on the account clock.
    env.clock.advance(Duration::from_secs(60));
    eventually("the reaction is gone", WAIT, || {
        let env = &env;
        async move { reactions_shown(env, conv).await.is_empty() }
    })
    .await;
    env.stop().await;
}

/// An empty removal of OUR account's other device (the phone) comes with a clock that runs ahead
/// of this computer's. A reaction made here afterwards must show, not turn into a removal at the
/// phone's time (it is stamped later than the removal it follows).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reaction_made_here_after_the_phones_removal_with_a_clock_ahead_shows() {
    let local = setup_message();
    let phone_removal = BackendEvent::Reaction {
        target: target(Some(&d_alice()), Some(&alice()), &local),
        by: None,
        emoji: String::new(),
        remove: true,
        at: T0 + 10 * 60_000,
    };
    let mut env = Env::new(
        Flavor::WhatsAppLike,
        script("reaction-clock").deliver(vec![phone_removal]),
    )
    .await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let message: MessageId = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [&local],
            |r| r.get(0).map(MessageId),
        )
        .expect("the message")
    });
    let own = |env: &Env| -> Vec<(String, bool)> {
        core_db(&env.core_db(), |c| {
            c.prepare(
                "SELECT emoji, removed FROM reactions WHERE message_id = ?1 AND user_id = ''
                 ORDER BY emoji",
            )
            .expect("prepare")
            .query_map([message.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
        })
    };
    // The phone's removal is stored: nothing of ours is showing, only its tombstone.
    assert_eq!(own(&env), [(String::new(), true)]);
    env.handle
        .send(UiCommand::React {
            message,
            emoji: "👍".into(),
            remove: false,
        })
        .expect("send");
    let conv = conv_id(&env.core_db(), &d_alice()).expect("the chat");
    eventually("our reaction shows", WAIT, || {
        let env = &env;
        async move { reactions_shown(env, conv).await == one("👍") }
    })
    .await;
    assert_eq!(
        own(&env),
        [(String::new(), true), ("👍".to_owned(), false)],
        "stamped after the removal it follows"
    );
    env.stop().await;
}

// ---------- the ingest on a real core.db ----------

/// A WhatsApp-like chat (one reaction per sender): the peer's reaction is taken back by the empty
/// removal and nobody else's is touched.
#[test]
fn an_empty_removal_takes_away_that_senders_reaction_and_no_ones_else() {
    let (_dir, mut h) = harness(true);
    h.apply(vec![
        reaction(WA_PEER, "👍", false, T0 + 1_000),
        reaction(WA_BOB, "👍", false, T0 + 2_000),
    ])
    .expect("reactions");
    assert_eq!(shown(&h), [("👍".to_owned(), 2)]);
    h.apply(vec![reaction(WA_PEER, "", true, T0 + 3_000)])
        .expect("the empty removal");
    assert_eq!(shown(&h), one("👍"), "bob's stays");
    assert_eq!(
        rows_of(&h, WA_PEER),
        [
            (String::new(), T0 + 3_000, true),
            ("👍".to_owned(), T0 + 3_000, true)
        ],
        "the removal is kept as a tombstone and the reaction became a removal at its time"
    );
    assert_eq!(rows_of(&h, WA_BOB), [("👍".to_owned(), T0 + 2_000, false)]);
}

/// In a set network (Threema: several emoji per sender) the empty removal takes away ALL of the
/// sender's emoji, and a removal WITH an emoji still takes that one only and leaves no
/// tombstone of the empty emoji.
#[test]
fn a_removal_with_an_emoji_takes_that_emoji_only_and_an_empty_one_all_of_them() {
    let (_dir, mut h) = harness(false);
    h.apply(vec![
        reaction(WA_PEER, "👍", false, T0 + 1_000),
        reaction(WA_PEER, "❤️", false, T0 + 2_000),
        reaction(WA_BOB, "❤️", false, T0 + 2_500),
    ])
    .expect("reactions");
    assert_eq!(shown(&h).len(), 2, "two emoji are showing");
    // With an emoji: that one only.
    h.apply(vec![reaction(WA_PEER, "👍", true, T0 + 3_000)])
        .expect("removal of one emoji");
    assert_eq!(shown(&h), [("❤️".to_owned(), 2)]);
    assert!(
        rows_of(&h, WA_PEER)
            .iter()
            .all(|(emoji, _, _)| !emoji.is_empty()),
        "no tombstone of the empty emoji"
    );
    // With none: every emoji of the sender, bob's untouched.
    h.apply(vec![reaction(WA_PEER, "", true, T0 + 4_000)])
        .expect("the empty removal");
    assert_eq!(shown(&h), one("❤️"));
    assert_eq!(
        rows_of(&h, WA_BOB),
        [("❤️".to_owned(), T0 + 2_500, false)],
        "bob's row is as it was"
    );
}

/// Signal's case, a one-reaction chat: the removal names the emoji and takes it away; nothing about
/// it changed (the rows are the ones a removal with an emoji always wrote).
#[test]
fn signals_removal_by_emoji_is_unchanged() {
    let (_dir, mut h) = harness(true);
    h.apply(vec![reaction(WA_PEER, "👍", false, T0 + 1_000)])
        .expect("reaction");
    assert_eq!(shown(&h), one("👍"));
    h.apply(vec![reaction(WA_PEER, "👍", true, T0 + 2_000)])
        .expect("removal");
    assert!(shown(&h).is_empty());
    assert_eq!(
        rows_of(&h, WA_PEER),
        [("👍".to_owned(), T0 + 2_000, true)],
        "one tombstone of that emoji, none of the empty one"
    );
    // A removal that is older than the reaction it names does not take it away.
    h.apply(vec![
        reaction(WA_PEER, "🎉", false, T0 + 5_000),
        reaction(WA_PEER, "🎉", true, T0 + 4_000),
    ])
    .expect("an older removal");
    assert_eq!(shown(&h), one("🎉"));
}

/// An empty removal with nothing to remove is a no-op: nothing the user sees changes, the UI is
/// told of no changed message, and another sender's reaction stays.
#[test]
fn an_empty_removal_with_nothing_to_remove_is_a_no_op() {
    let (_dir, mut h) = harness(true);
    let before = shown(&h);
    let told = h
        .apply_announcing(vec![reaction(WA_PEER, "", true, T0 + 1_000)])
        .expect("the empty removal");
    assert!(told.changed.is_empty(), "no message changed: {told:?}");
    assert_eq!(shown(&h), before);
    assert!(before.is_empty());
    // Bob reacted; an empty removal of the peer, who has nothing there, leaves it and tells nobody.
    h.apply(vec![reaction(WA_BOB, "👍", false, T0 + 2_000)])
        .expect("bob");
    let told = h
        .apply_announcing(vec![reaction(WA_PEER, "", true, T0 + 3_000)])
        .expect("the empty removal again");
    assert!(told.changed.is_empty(), "no message changed: {told:?}");
    assert_eq!(shown(&h), one("👍"));
    // An empty emoji that does not remove is no reaction at all.
    let rows = count_reactions(&h);
    h.apply(vec![reaction(WA_PEER, "", false, T0 + 4_000)])
        .expect("an empty reaction");
    assert_eq!(count_reactions(&h), rows, "nothing was stored");
}

/// The removal keeps its place in time, so the arrival order does not matter: a reaction, the empty
/// removal after it and a newer reaction end in the same rows in every order, with every event
/// delivered twice too, and the newer reaction is the one showing.
#[test]
fn the_empty_removal_converges_in_every_order_and_a_newer_reaction_stands() {
    let events = [
        reaction(WA_PEER, "👍", false, T0 + 1_000),
        reaction(WA_PEER, "", true, T0 + 2_000),
        reaction(WA_PEER, "❤️", false, T0 + 3_000),
    ];
    let orders: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut dumps: Vec<(String, String)> = Vec::new();
    for order in orders {
        for twice in [false, true] {
            let (_dir, mut h) = harness(true);
            for i in order {
                h.apply(vec![events[i].clone()]).expect("event");
                if twice {
                    h.apply(vec![events[i].clone()]).expect("event again");
                }
            }
            let name = format!("{order:?}, twice {twice}");
            assert_eq!(shown(&h), one("❤️"), "{name}");
            dumps.push((name, h.dump(DumpMode::OrderInsensitive).expect("dump")));
        }
    }
    let (first_name, first) = &dumps[0];
    for (name, dump) in &dumps[1..] {
        assert_eq!(dump, first, "{name} against {first_name}");
    }
    // And the removal alone, whichever side of the reaction it falls, takes the older one away.
    for removal_first in [false, true] {
        let (_dir, mut h) = harness(true);
        let (a, b) = (events[0].clone(), events[1].clone());
        let order = if removal_first { [b, a] } else { [a, b] };
        for e in order {
            h.apply(vec![e]).expect("event");
        }
        assert!(shown(&h).is_empty(), "removal first {removal_first}");
    }
}

/// The merge of two identities of one person brings the empty removal along: the number's removal
/// takes away the LID's older reaction, and a reaction the number made before is taken too.
#[test]
fn a_merge_carries_the_empty_removal_to_the_canonical_identity() {
    let (_dir, mut h) = harness(true);
    let alias = "pn:15550000111";
    h.apply(vec![
        reaction(WA_PEER, "👍", false, T0 + 1_000),
        reaction(alias, "❤️", false, T0 + 1_500),
        reaction(alias, "", true, T0 + 2_000),
        BackendEvent::IdentityMerged {
            from: wa_user(alias),
            into: wa_user(WA_PEER),
        },
    ])
    .expect("reactions and the merge");
    assert!(
        shown(&h).is_empty(),
        "both of the person's reactions are gone: {:?}",
        rows_of(&h, WA_PEER)
    );
}
