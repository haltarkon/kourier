//! `CoreSource`: message actions (edit, delete, react, reply, retry), quotes and jumps, the
//! disappearing-message timer, list previews and the account-state banner, against the
//! simulated core of [`super::tests`].

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kourier_core::facade::{
    ActionSubject, MessageActions, Preview, Query, QueryData, QuoteState as CoreQuoteState,
    ReactionGroup, ReplyPreview, TimelineAnchor, UiCommand, UiEvent,
};
use kourier_model::{
    AccountId, AccountState, AccountUuid, AttachmentId, AttachmentKind, BlobKey, BlobRef, Body,
    CannotSend, ConnState, ConversationId, DisappearKind, Expiry, FailReason, LoginStep,
    LoginStepKind, MessageId, OutgoingState, ReadState, Redacted, SystemEvent, Text, UserAction,
};

use super::tests::{
    NOW, Sent, Store, account, message, result, settle, source, take_queries, user,
};
use super::*;
use crate::conversations::BannerKind;
use crate::timeline::{RowKey, RowKind};

const CONV: ConversationId = ConversationId(1);

fn id(n: i64) -> MessageId {
    MessageId(100_000 + n)
}

/// What the core offers for an own recent text.
fn own_actions() -> MessageActions {
    MessageActions {
        reply: true,
        react: true,
        edit: true,
        delete_for_everyone: true,
        delete_for_me: true,
        delete_attachment: false,
        copy: true,
        take_back: false,
        ask_attachments: Vec::new(),
        until: None,
    }
}

/// What it offers for someone else's text.
fn peer_actions() -> MessageActions {
    MessageActions {
        edit: false,
        delete_for_everyone: false,
        ..own_actions()
    }
}

/// A conversation of `n` messages (every third is our own) with the usual flags.
fn store(n: i64) -> Store {
    let mut store = Store::new(&[(1, n)]);
    for m in store.convs.get_mut(&1).expect("conv") {
        m.actions = if m.author.is_none() {
            own_actions()
        } else {
            peer_actions()
        };
    }
    store
}

/// A source with conversation 1 of `store` open and loaded.
fn opened(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, store);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, store);
    (src, sent)
}

/// Takes every command sent so far.
fn commands(sent: &Sent) -> Vec<UiCommand> {
    std::mem::take(&mut *sent.borrow_mut())
}

fn row(src: &CoreSource, message: MessageId) -> MessageRow {
    src.timeline(CONV)
        .into_iter()
        .find(|r| r.key == RowKey::Message(message))
        .expect("row")
}

#[test]
fn rows_carry_the_cores_flags_and_say_what_a_reply_or_an_edit_starts_from() {
    let mut store = store(6);
    {
        let rows = store.convs.get_mut(&1).expect("conv");
        // A send that failed can be sent again; one nobody has yet is taken back.
        rows[2].out_state = Some(OutgoingState::Failed {
            reason: FailReason::Network,
            retryable: true,
        });
        rows[2].actions = MessageActions {
            delete_for_me: true,
            take_back: true,
            ..MessageActions::default()
        };
        // A tombstone offers only its removal from this account.
        rows[3].deleted = Some(kourier_model::Deletion::ForEveryone { by: Some(user(1)) });
        rows[3].actions = MessageActions {
            delete_for_me: true,
            ..MessageActions::default()
        };
    }
    let (src, _) = opened(&store);

    let own = row(&src, id(6));
    assert!(own.own && own.actions.edit && own.actions.delete_for_everyone);
    assert!(own.actions.reply && own.actions.react && own.actions.copy && !own.actions.retry);
    let peer = row(&src, id(5));
    assert!(!peer.actions.edit && !peer.actions.delete_for_everyone && peer.actions.reply);
    let failed = row(&src, id(3));
    assert!(failed.actions.retry && failed.actions.take_back && !failed.actions.reply);
    let gone = row(&src, id(4));
    assert_eq!(
        gone.actions,
        RowActions {
            delete_for_me: true,
            ..RowActions::default()
        }
    );
    assert!(gone.actions.any() && !RowActions::default().any());

    // A reply quotes what the row shows; a tombstone cannot be quoted.
    let quote = src.reply_target(CONV, id(5)).expect("quote");
    assert_eq!(
        (
            quote.author.as_str(),
            quote.text.as_str(),
            quote.own,
            quote.target
        ),
        ("Ari Arden", "1:5", false, Some(id(5)))
    );
    assert!(src.reply_target(CONV, id(4)).is_none());
    assert!(src.reply_target(ConversationId(9), id(5)).is_none());

    // An edit starts from the text; only what the core lets us edit.
    let edit = src.edit_target(CONV, id(6)).expect("edit");
    assert_eq!((edit.text.as_str(), edit.caption), ("1:6", false));
    assert!(src.edit_target(CONV, id(5)).is_none(), "not our message");
    assert!(!format!("{edit:?}").contains("1:6"), "no text in logs");
    // Up in an empty composer: the newest own message that can be edited.
    assert_eq!(src.last_editable(CONV), Some(id(6)));
}

#[test]
fn up_in_an_empty_composer_skips_what_can_no_longer_be_edited() {
    let mut store = store(6);
    store.convs.get_mut(&1).expect("conv")[5].actions.edit = false;
    let (src, _) = opened(&store);
    // Message 3 is the next own one.
    assert_eq!(src.last_editable(CONV), Some(id(3)));
    let mut store = store.clone();
    store.convs.get_mut(&1).expect("conv")[2].actions.edit = false;
    let (src, _) = opened(&store);
    assert_eq!(src.last_editable(CONV), None);
}

#[test]
fn an_edit_sends_the_new_text_once_it_differs() {
    let (mut src, sent) = opened(&store(6));
    commands(&sent);
    // The same text (trailing white space aside): nothing is sent.
    assert_eq!(src.edit_message(CONV, id(6), "1:6 \n"), Ok(false));
    // A text cannot become empty.
    assert_eq!(
        src.edit_message(CONV, id(6), " \n"),
        Err(SourceError::Empty)
    );
    // Not ours to edit.
    assert!(matches!(
        src.edit_message(CONV, id(5), "x"),
        Err(SourceError::CannotSend(_))
    ));
    assert!(commands(&sent).is_empty());
    assert_eq!(src.edit_message(CONV, id(6), "fixed  "), Ok(true));
    match &commands(&sent)[..] {
        [UiCommand::EditMessage { message, text }] => {
            assert_eq!((*message, text.body.as_str()), (id(6), "fixed"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_edit_keeps_the_formatting_and_mentions_it_did_not_touch() {
    use kourier_model::text::MENTION_PLACEHOLDER as P;
    use kourier_model::{RangeKind, TextRange};
    let range = |start: usize, len: usize, kind: RangeKind| TextRange {
        start: start as u32,
        len: len as u32,
        kind,
    };
    // "Hi @…, the code is [1234]. Dont share" from our phone: a mention, a hidden code, bold.
    let body = format!("Hi {P}, the code is 1234. Dont share");
    let code = body.find("1234").expect("code");
    let bold = body.find("Dont share").expect("bold");
    let original = Text {
        body: body.clone(),
        ranges: vec![
            range(3, P.len_utf8(), RangeKind::Mention(user(1))),
            range(code, 4, RangeKind::Spoiler),
            range(bold, 10, RangeKind::Bold),
        ],
    };
    let mut store = store(6);
    store.convs.get_mut(&1).expect("conv")[5].body = Body::Text(original.clone());
    let (mut src, sent) = opened(&store);
    commands(&sent);
    // The composer gets the text with the mention written out (the name the timeline shows).
    let name = src.names.display_name(&user(1));
    let edit = src.edit_target(CONV, id(6)).expect("edit");
    assert_eq!(
        edit.text,
        format!("Hi @{name}, the code is 1234. Dont share")
    );
    assert!(!edit.caption);
    // Saved untouched: nothing is sent.
    assert_eq!(src.edit_message(CONV, id(6), &edit.text), Ok(false));
    assert!(commands(&sent).is_empty());
    // A typo fixed at the end: the mention is still a mention, the code is still hidden.
    let fixed = format!("Hi @{name}, the code is 1234. Don't share");
    assert_eq!(src.edit_message(CONV, id(6), &fixed), Ok(true));
    let sent_text = match &commands(&sent)[..] {
        [UiCommand::EditMessage { message, text }] if *message == id(6) => text.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        sent_text.body,
        format!("Hi {P}, the code is 1234. Don't share")
    );
    let part = |kind: &RangeKind| {
        sent_text
            .ranges
            .iter()
            .find(|r| &r.kind == kind)
            .map(|r| &sent_text.body[r.start as usize..(r.start + r.len) as usize])
    };
    assert_eq!(part(&RangeKind::Mention(user(1))), Some("\u{fffc}"));
    assert_eq!(part(&RangeKind::Spoiler), Some("1234"));
    assert_eq!(part(&RangeKind::Bold), Some("Don't share"));
    assert_eq!(sent_text.ranges.len(), 3);
    // The hidden code itself is corrected: it stays hidden.
    let recoded = format!("Hi @{name}, the code is 12345. Dont share");
    assert_eq!(src.edit_message(CONV, id(6), &recoded), Ok(true));
    match &commands(&sent)[..] {
        [UiCommand::EditMessage { text, .. }] => {
            let spoiler = text
                .ranges
                .iter()
                .find(|r| r.kind == RangeKind::Spoiler)
                .expect("the spoiler");
            assert_eq!(
                &text.body[spoiler.start as usize..(spoiler.start + spoiler.len) as usize],
                "12345"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_caption_can_be_edited_and_removed() {
    use kourier_model::{Attachment, AttachmentState};
    let mut store = store(6);
    store.convs.get_mut(&1).expect("conv")[5].body = Body::Media {
        attachments: vec![Attachment {
            id: Some(AttachmentId(11)),
            kind: AttachmentKind::Image,
            mime: "image/jpeg".into(),
            state: AttachmentState::Local,
            ..Attachment::default()
        }],
        caption: Some(Text::plain("The view")),
    };
    let (mut src, sent) = opened(&store);
    commands(&sent);
    let edit = src.edit_target(CONV, id(6)).expect("edit");
    assert_eq!((edit.text.as_str(), edit.caption), ("The view", true));
    assert_eq!(src.edit_message(CONV, id(6), "The view, later"), Ok(true));
    // An empty text removes the caption (the picture stays).
    assert_eq!(src.edit_message(CONV, id(6), ""), Ok(true));
    let texts: Vec<String> = commands(&sent)
        .into_iter()
        .filter_map(|c| match c {
            UiCommand::EditMessage { text, .. } => Some(text.body),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["The view, later", ""]);
}

#[test]
fn delete_react_retry_cancel_and_the_timer_send_their_commands() {
    let (mut src, sent) = opened(&store(6));
    commands(&sent);
    assert!(src.delete_message(id(6), true));
    assert!(src.delete_message(id(5), false));
    assert!(src.react(id(5), "👍", false));
    assert!(src.react(id(5), "👍", true));
    assert!(!src.react(id(5), "  ", false), "no emoji, no command");
    assert!(src.retry_message(id(3)));
    assert!(src.cancel_message(id(3)));
    assert!(src.set_timer(CONV, 3600));
    assert!(src.set_timer(CONV, 0));
    assert!(src.restart_account(AccountId(1)));
    assert!(src.submit_challenge(AccountId(1), " token-from-the-browser \n"));
    assert!(
        !src.submit_challenge(AccountId(1), "  "),
        "nothing to submit"
    );
    let cmds = commands(&sent);
    assert!(matches!(
        &cmds[..],
        [
            UiCommand::DeleteMessage { message: a, for_everyone: true },
            UiCommand::DeleteMessage { message: b, for_everyone: false },
            UiCommand::React { message: c, emoji: e1, remove: false },
            UiCommand::React { emoji: e2, remove: true, .. },
            UiCommand::RetryMessage { message: d },
            UiCommand::CancelMessage { message: e },
            UiCommand::SetConversationTimer { conv: CONV, timer: Some(t) },
            UiCommand::SetConversationTimer { timer: None, .. },
            UiCommand::RestartAccount { account: AccountId(1) },
            UiCommand::SubmitChallenge { account: AccountId(1), answer },
        ] if *a == id(6) && *b == id(5) && *c == id(5) && e1 == "👍" && e2 == "👍"
            && *d == id(3) && *e == id(3) && *t == Duration::from_secs(3600)
            && answer.expose() == "token-from-the-browser"
    ));
    // The answer is a secret: `Debug` of the command does not show it.
    assert!(!format!("{:?}", cmds[9]).contains("token-from"));
}

/// The network's final no to a request that was sent.
fn network_no() -> CoreError {
    kourier_model::BackendError::new(kourier_model::ErrorKind::InvalidInput, "refused").into()
}

/// The core's own refusal. Its wording is deliberately none the core uses: notices must not
/// depend on it.
fn core_no() -> CoreError {
    CoreError::InvalidInput("reworded")
}

#[test]
fn refused_actions_show_a_notice_chosen_by_kind_and_row_state() {
    let mut store = store(12);
    {
        let rows = store.convs.get_mut(&1).expect("conv");
        // 6: an own text whose edit and delete windows ended.
        rows[5].actions = MessageActions {
            edit: false,
            delete_for_everyone: false,
            ..own_actions()
        };
        // 3: an own text that was deleted meanwhile.
        rows[2].deleted = Some(kourier_model::Deletion::ForEveryone { by: None });
        rows[2].actions = MessageActions {
            delete_for_me: true,
            ..MessageActions::default()
        };
        // 9: an own text whose transmit is in flight.
        rows[8].out_state = Some(OutgoingState::Sending { progress: None });
        rows[8].actions = MessageActions {
            delete_for_me: false,
            delete_for_everyone: false,
            ..own_actions()
        };
    }
    let (mut src, sent) = opened(&store);
    let (old, deleted, sending, peer) = (id(6), id(3), id(9), id(5));
    for (action, error, about, notice) in [
        (
            "EditMessage",
            core_no(),
            old,
            "This message can no longer be edited.",
        ),
        (
            "EditMessage",
            core_no(),
            deleted,
            "This message was deleted.",
        ),
        (
            "EditMessage",
            core_no(),
            id(999),
            "This message can no longer be edited.",
        ),
        (
            "EditMessage",
            network_no(),
            old,
            "The edit could not be sent. The message is as it was.",
        ),
        (
            "DeleteMessage",
            core_no(),
            sending,
            "This message is being sent. Delete it once it is.",
        ),
        (
            "DeleteMessage",
            core_no(),
            old,
            "This message can no longer be deleted for everyone.",
        ),
        (
            "DeleteMessage",
            core_no(),
            id(999),
            "This message cannot be deleted.",
        ),
        (
            "DeleteMessage",
            network_no(),
            old,
            "The network did not delete the message for everyone. It is deleted on your devices only.",
        ),
        (
            "React",
            core_no(),
            peer,
            "This reaction is not possible here.",
        ),
        ("React", core_no(), deleted, "This message was deleted."),
        (
            "React",
            network_no(),
            peer,
            "The reaction could not be sent.",
        ),
        (
            "CancelMessage",
            core_no(),
            sending,
            "The message is already on its way and can no longer be taken back.",
        ),
        (
            "RetryMessage",
            core_no(),
            old,
            "The message could not be sent.",
        ),
        (
            "RetryMessage",
            kourier_model::BackendError::new(
                kourier_model::ErrorKind::RateLimited { retry_after: None },
                "429",
            )
            .into(),
            old,
            "The network asks to slow down. Try again in a little while.",
        ),
    ] {
        src.clear_notice();
        let ch = src.on_action_failed(action, &error, &ActionSubject::Message(about));
        assert!(ch.notice, "{action} on {about:?}");
        assert_eq!(src.notice(), notice, "{action} on {about:?}");
    }
    for (action, error, notice) in [
        (
            "SetConversationTimer",
            core_no(),
            "This timer is not available in this conversation.",
        ),
        (
            "SetConversationTimer",
            network_no(),
            "The disappearing-message timer could not be changed.",
        ),
        (
            "SubmitChallenge",
            network_no(),
            "The verification was not accepted. Try it again from the banner.",
        ),
    ] {
        let ch = src.on_action_failed(action, &error, &ActionSubject::Conversation(CONV));
        assert!(ch.notice);
        assert_eq!(src.notice(), notice);
    }
    // The chat allows a number of edits and this message used them up.
    {
        let mut store = store.clone();
        store.features.edit = Some(kourier_model::Window {
            max_age_secs: None,
            max_count: Some(2),
        });
        let rows = store.convs.get_mut(&1).expect("conv");
        rows[5].revision = 2;
        let (mut src, _) = opened(&store);
        src.on_action_failed("EditMessage", &core_no(), &ActionSubject::Message(old));
        assert_eq!(
            src.notice(),
            "This message was edited too often to change it again."
        );
    }
    // A refusal for a row that still offers the edit says nothing (the text was what it was,
    // or the window ended this moment): the rows are read again instead.
    src.clear_notice();
    commands(&sent);
    let ch = src.on_action_failed("EditMessage", &core_no(), &ActionSubject::Message(id(12)));
    assert!(!ch.any() && src.notice().is_empty());
    assert!(
        commands(&sent)
            .iter()
            .any(|c| matches!(c, UiCommand::Query { .. })),
        "the timeline is read again"
    );
    // A reply to a message that is gone meanwhile: the quote is what was refused.
    src.send_text(CONV, "an answer", Some(deleted))
        .expect("send");
    let ch = src.on_action_failed("SendText", &core_no(), &ActionSubject::Conversation(CONV));
    assert!(ch.notice && ch.draft);
    assert_eq!(
        src.notice(),
        "The message you reply to is no longer there. Send it without the quote."
    );
    assert_eq!(src.draft(CONV), "an answer");
    // The same refusal of a reply whose original is fine is not blamed on the quote.
    src.save_draft(CONV, "");
    src.send_text(CONV, "another", Some(peer)).expect("send");
    src.on_action_failed("SendText", &core_no(), &ActionSubject::Conversation(CONV));
    assert_eq!(src.notice(), "The message could not be sent.");
    // A new action clears the old notice.
    src.on_action_failed("React", &network_no(), &ActionSubject::Message(peer));
    assert!(src.react(id(5), "👍", false));
    assert!(src.notice().is_empty());
}

fn quote(src: &CoreSource, message: MessageId) -> QuoteBlock {
    match row(src, message).blocks.first() {
        Some(Block::Quote(q)) => q.clone(),
        other => panic!("not a quote: {other:?}"),
    }
}

#[test]
fn quotes_show_what_the_core_resolved() {
    use kourier_model::{Attachment, AttachmentState};
    let mut store = store(8);
    {
        let rows = store.convs.get_mut(&1).expect("conv");
        // The original of the first quote: a photo with a caption.
        rows[0].body = Body::Media {
            attachments: vec![Attachment {
                id: Some(AttachmentId(11)),
                kind: AttachmentKind::Image,
                mime: "image/jpeg".into(),
                size: 48_000,
                state: AttachmentState::Local,
                ..Attachment::default()
            }],
            caption: Some(Text::plain("The view")),
        };
        let reply = |state, target: Option<i64>| ReplyPreview {
            target: target.map(id),
            state,
            author: Some(user(1)),
            own: false,
            author_name: Some(Redacted::new("Ari Arden".to_owned())),
            snapshot: None,
            text: None,
            attachment: None,
            file_name: None,
            thumb: None,
        };
        // Known, with the original's caption and its thumbnail.
        rows[2].reply = Some(ReplyPreview {
            text: Some(Text::plain("The view\nfrom the ridge")),
            attachment: Some(AttachmentKind::Image),
            thumb: Some(AttachmentId(11)),
            ..reply(CoreQuoteState::Known, Some(1))
        });
        // Not on this device, and the quote carried nothing.
        rows[3].reply = Some(reply(CoreQuoteState::Missing, None));
        // Deleted after it was quoted: the tombstone can be jumped to, nothing of it shows.
        rows[4].reply = Some(ReplyPreview {
            text: Some(Text::plain("what it said")),
            thumb: Some(AttachmentId(11)),
            ..reply(CoreQuoteState::Deleted, Some(2))
        });
        // Our own file, quoted by its name.
        rows[5].reply = Some(ReplyPreview {
            own: true,
            author: None,
            author_name: None,
            attachment: Some(AttachmentKind::File),
            file_name: Some(Redacted::new("Plan.pdf".to_owned())),
            ..reply(CoreQuoteState::Known, Some(3))
        });
        // A voice note of someone the core has no name for.
        rows[6].reply = Some(ReplyPreview {
            author_name: None,
            author: Some(user(0x1f2e)),
            attachment: Some(AttachmentKind::Voice),
            ..reply(CoreQuoteState::Missing, None)
        });
    }
    let (src, _) = opened(&store);

    let q = quote(&src, id(3));
    assert_eq!(
        (q.author.as_str(), q.text.as_str(), q.icon.as_str()),
        ("Ari Arden", "The view from the ridge", "photo")
    );
    assert_eq!(
        (q.state, q.target, q.own),
        (QuoteState::Known, Some(id(1)), false)
    );
    let (base, rev) = q.thumb_url.split_once("?v=").expect("a revision");
    assert_eq!(base, "image://kourier/thumb/11");
    assert_ne!(rev, "0", "the revision follows the loaded attachment");

    let q = quote(&src, id(4));
    assert_eq!(
        (q.text.as_str(), q.icon.as_str(), q.state, q.target),
        (
            "Original message not found",
            "dialog-question",
            QuoteState::Missing,
            None
        )
    );
    assert!(q.thumb_url.is_empty());

    let q = quote(&src, id(5));
    assert_eq!(
        (q.text.as_str(), q.icon.as_str(), q.state, q.target),
        (
            "Deleted message",
            "trash-empty",
            QuoteState::Deleted,
            Some(id(2))
        )
    );
    assert!(
        q.thumb_url.is_empty(),
        "nothing of a deleted original shows"
    );

    let q = quote(&src, id(6));
    assert_eq!(
        (q.author.as_str(), q.text.as_str(), q.icon.as_str(), q.own),
        ("You", "Plan.pdf", "mail-attachment", true)
    );

    let q = quote(&src, id(7));
    assert_eq!(q.text, "Voice message");
    assert!(q.author.contains("1f2e"), "a short id, never the raw one");
}

#[test]
fn a_quote_jumps_to_its_original_and_loads_the_window_around_it() {
    let store = store(200);
    let (mut src, sent) = opened(&store);
    assert_eq!(src.window_len(), TIMELINE_PAGE as usize);
    // Loaded: the view only has to scroll.
    let ch = src.jump_to(id(190));
    assert_eq!(ch.jump, Some(id(190)));
    assert!(take_queries(&sent).is_empty());

    // Not loaded: the window moves to the stretch around it.
    let ch = src.jump_to(id(20));
    assert_eq!(ch.jump, None, "not before it is among the rows");
    let queries = take_queries(&sent);
    assert!(matches!(
        &queries[..],
        [(_, Query::Timeline(q))] if q.anchor == TimelineAnchor::Around(id(20))
    ));
    let mut ch = Changes::default();
    for (t, q) in queries {
        ch.merge(src.on_event(&result(t, store.answer(&q))));
    }
    assert!(ch.timeline);
    assert_eq!(ch.jump, Some(id(20)));
    assert!(
        src.timeline(CONV)
            .iter()
            .any(|r| r.key == RowKey::Message(id(20)))
    );
    let paging = src.paging();
    assert!(paging.has_newer, "the newest messages are not loaded now");

    // An invalidation re-reads the stretch that is shown, not the newest page.
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![id(20)],
    });
    settle(&mut src, &sent, &store);
    assert!(
        src.timeline(CONV)
            .iter()
            .any(|r| r.key == RowKey::Message(id(20)))
    );

    // "Jump to latest" loads the newest page again.
    src.jump_to_latest();
    let queries = take_queries(&sent);
    assert!(matches!(
        &queries[..],
        [(_, Query::Timeline(q))] if q.anchor == TimelineAnchor::Newest
    ));
    for (t, q) in queries {
        src.on_event(&result(t, store.answer(&q)));
    }
    settle(&mut src, &sent, &store);
    assert!(!src.paging().has_newer);
    assert_eq!(
        src.timeline(CONV).first().map(|r| r.key),
        Some(RowKey::Message(id(200)))
    );
    // At the newest message there is nothing to load.
    src.jump_to_latest();
    assert!(take_queries(&sent).is_empty());

    // The original went away meanwhile: a notice, and the window stays.
    src.jump_to(MessageId(999));
    let ch = settle(&mut src, &sent, &store);
    assert!(ch.notice && ch.jump.is_none());
    assert_eq!(src.notice(), "The original message is no longer available.");
    assert_eq!(
        src.timeline(CONV).first().map(|r| r.key),
        Some(RowKey::Message(id(200)))
    );
}

fn has_divider(src: &CoreSource) -> bool {
    src.timeline(CONV)
        .iter()
        .any(|r| r.kind == RowKind::UnreadMarker)
}

#[test]
fn the_unread_divider_goes_once_we_answered() {
    let mut store = Store::new(&[(1, 5)]);
    for m in store.convs.get_mut(&1).expect("conv").iter_mut().skip(3) {
        m.read_state = Some(ReadState::Unread);
    }
    // Our reply from this device: the divider goes with the send, before the row is there.
    let (mut src, _) = opened(&store);
    assert!(has_divider(&src), "two unread messages");
    src.send_text(CONV, "on my way", None).expect("send");
    assert!(!has_divider(&src));

    // A picture as the answer does the same.
    let (mut src, _) = opened(&store);
    src.send_files(CONV, vec!["/tmp/a.jpg".into()], Vec::new(), "", None)
        .expect("send");
    assert!(!has_divider(&src));

    // An answer from our phone: the divider goes when its row arrives.
    let (mut src, sent) = opened(&store);
    assert!(has_divider(&src));
    let mut answered = store.clone();
    answered
        .convs
        .get_mut(&1)
        .expect("conv")
        .push(message(1, 6, false));
    src.on_event(&UiEvent::MessagesChanged {
        conv: CONV,
        ids: vec![id(6)],
    });
    settle(&mut src, &sent, &answered);
    assert!(!has_divider(&src));

    // A chat we answered before it was opened shows no divider at all, while more unread
    // messages after our last one still get theirs.
    let (src, _) = opened(&answered);
    assert!(!has_divider(&src));
    let mut more = answered.clone();
    let mut late = message(1, 7, true);
    late.read_state = Some(ReadState::Unread);
    more.convs.get_mut(&1).expect("conv").push(late);
    let (src, _) = opened(&more);
    assert!(has_divider(&src));
}

fn preview(body_kind: &str) -> Preview {
    Preview {
        message: MessageId(1),
        author: Some(user(1)),
        author_name: Some("Ari Arden".into()),
        body_kind: body_kind.into(),
        text: None,
        sent_at: NOW,
        deleted: false,
        out_state: None,
        attachment: None,
        attachments: 0,
        file_name: None,
        system: None,
        kind_hint: None,
    }
}

#[test]
fn list_previews_say_what_the_newest_message_is() {
    let mut names = NameMap::new();
    names.insert(user(1), "Ari Arden");
    names.insert(user(2), "Bo Birch");
    let show = |p: &Preview| {
        let s = core_preview(p, &names);
        (s.text, s.icon)
    };
    let pair = |text: &str, icon: &str| (text.to_owned(), icon.to_owned());
    let media = |kind, count| Preview {
        attachment: Some(kind),
        attachments: count,
        ..preview("Media")
    };
    assert_eq!(
        show(&media(AttachmentKind::Voice, 1)),
        pair("Voice message", "media-record")
    );
    assert_eq!(
        show(&media(AttachmentKind::Image, 1)),
        pair("Photo", "photo")
    );
    assert_eq!(
        show(&media(AttachmentKind::Image, 3)),
        pair("3 photos", "photo")
    );
    assert_eq!(
        show(&media(AttachmentKind::Video, 1)),
        pair("Video", "video-x-generic")
    );
    assert_eq!(
        show(&media(AttachmentKind::File, 1)),
        pair("File", "mail-attachment")
    );
    // A caption says more than the kind; the icon still tells what it captions.
    assert_eq!(
        show(&Preview {
            text: Some("Sunset\nover the ridge".into()),
            ..media(AttachmentKind::Image, 1)
        }),
        pair("Sunset over the ridge", "photo")
    );
    assert_eq!(
        show(&Preview {
            text: Some("plain".into()),
            ..preview("Text")
        }),
        pair("plain", "")
    );
    // What happened, in words.
    assert_eq!(
        show(&Preview {
            system: Some(SystemEvent::MembersRemoved {
                by: Some(user(1)),
                users: vec![user(2)],
            }),
            ..preview("System")
        }),
        pair("Ari Arden removed Bo Birch", "list-remove-user")
    );
    assert_eq!(
        show(&Preview {
            system: Some(SystemEvent::TimerChanged {
                by: None,
                timer: Some(Duration::from_secs(86_400)),
            }),
            ..preview("System")
        }),
        pair("You set the disappearing message timer to 1 day", "clock")
    );
    assert_eq!(
        show(&preview("System")),
        pair("Conversation updated", "dialog-information")
    );
    // A chat whose newest row is a call line says what the call is or became.
    let call = |group, state, by: Option<UserId>| {
        use kourier_model::{CallDirection, CallLine, CallMedia};
        let mut line = CallLine::signal(CallDirection::Incoming, group, state, 5);
        if !group {
            line = line.with_media(CallMedia::Video, 5);
        }
        if let Some(by) = by {
            line = line.with_by(by, 5);
        }
        Preview {
            system: Some(SystemEvent::Call(line)),
            ..preview("System")
        }
    };
    use kourier_model::CallState;
    assert_eq!(
        show(&call(
            false,
            CallState::Ringing { deadline: 9 },
            Some(user(1))
        )),
        pair("Incoming video call…", "call-incoming")
    );
    assert_eq!(
        show(&call(false, CallState::Missed, Some(user(1)))),
        pair("Missed video call", "call-missed")
    );
    assert_eq!(
        show(&call(true, CallState::Started, Some(user(2)))),
        pair("Bo Birch started a group call", "call-start")
    );
    // Content Kourier does not show yet, by its kind.
    let unsupported = |hint: &str| Preview {
        kind_hint: Some(hint.into()),
        ..preview("Unsupported")
    };
    assert_eq!(
        show(&unsupported("view-once")),
        pair("View-once media", "view-hidden")
    );
    assert_eq!(
        show(&unsupported("payment")),
        pair("Payment", "wallet-closed")
    );
    assert_eq!(
        show(&unsupported("poll-ended")),
        pair("Poll ended", "view-statistics")
    );
    assert_eq!(
        show(&unsupported("pin")),
        pair("Pinned message", "window-pin")
    );
    assert_eq!(
        show(&unsupported("something-new")),
        pair("Unsupported message", "help-about")
    );
    assert_eq!(
        show(&Preview {
            deleted: true,
            ..media(AttachmentKind::Image, 1)
        }),
        pair("This message was deleted", "trash-empty")
    );
}

#[test]
fn stand_ins_say_what_the_content_is_and_where_to_see_it() {
    let mut store = store(6);
    {
        let rows = store.convs.get_mut(&1).expect("conv");
        let unsupported = |hint: &str, summary: Option<&str>| Body::Unsupported {
            kind_hint: hint.into(),
            summary: summary.map(str::to_owned),
        };
        rows[0].body = unsupported("view-once", None);
        rows[1].body = unsupported("poll", Some("Friday or\nSaturday?"));
        rows[3].body = unsupported("made-up-kind", None);
        rows[4].body = unsupported("newer-protocol", None);
        rows[2].body = unsupported("poll-ended", None);
        rows[5].body = unsupported("pin", None);
    }
    let (src, _) = opened(&store);
    let text = |n: i64| match row(&src, id(n)).blocks.first() {
        Some(Block::Placeholder(s)) => (s.text.clone(), s.icon.clone()),
        other => panic!("not a stand-in: {other:?}"),
    };
    assert_eq!(
        text(1),
        (
            "View-once media. Open it on your phone.".to_owned(),
            "view-hidden".to_owned()
        )
    );
    assert_eq!(text(2).0, "Poll. Vote on your phone. Friday or Saturday?");
    assert_eq!(text(4).0, "This kind of message cannot be shown here yet.");
    assert_eq!(text(5).0, "This message needs a newer version of Kourier.");
    // Kinds that used to get the general text.
    assert_eq!(
        text(3),
        (
            "A poll ended. See the result on your phone.".to_owned(),
            "view-statistics".to_owned()
        )
    );
    assert_eq!(
        text(6),
        (
            "A message was pinned. See it on your phone.".to_owned(),
            "window-pin".to_owned()
        )
    );
}

#[test]
fn reactions_list_who_reacted_and_timers_are_marked() {
    let mut store = store(6);
    {
        let rows = store.convs.get_mut(&1).expect("conv");
        rows[4].reactions = vec![
            ReactionGroup {
                emoji: "❤️".into(),
                users: vec![Some(user(1))],
            },
            ReactionGroup {
                emoji: "👍".into(),
                users: vec![Some(user(1)), None, Some(user(0x2a2b))],
            },
        ];
        rows[4].expiry = Some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: Duration::from_secs(3600),
            started_at: Some(NOW),
        });
        rows[5].body = Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: Some(Duration::from_secs(3600)),
        });
        rows[5].actions = MessageActions {
            delete_for_me: true,
            ..MessageActions::default()
        };
    }
    let (src, _) = opened(&store);
    let r = row(&src, id(5));
    let chips: Vec<(&str, u32, bool, Vec<&str>)> = r
        .reactions
        .iter()
        .map(|c| {
            (
                c.emoji.as_str(),
                c.count,
                c.mine,
                c.names.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(chips[0].0, "👍");
    assert_eq!((chips[0].1, chips[0].2), (3, true));
    assert_eq!(chips[0].3[..2], ["Ari Arden", "You"]);
    assert!(
        chips[0].3[2].contains("2a2b"),
        "a short id for a nameless user"
    );
    assert_eq!(chips[1], ("❤️", 1, false, vec!["Ari Arden"]));
    assert_eq!(r.expiry, "Disappears 1 hour after it is read");
    // Our own timer change is a notice row that can only be removed from this account.
    let notice = row(&src, id(6));
    assert_eq!(notice.kind, RowKind::System);
    assert_eq!(
        notice.text,
        "You set the disappearing message timer to 1 hour"
    );
    assert!(notice.actions.delete_for_me && !notice.actions.reply && !notice.actions.react);
}

#[test]
fn the_header_offers_the_conversations_timers() {
    let mut store = store(3);
    store.features.disappearing = BTreeMap::from([(
        DisappearKind::AfterRead,
        vec![604_800, 30, 3600, 86_400, 30],
    )]);
    store.timer = Some(Duration::from_secs(86_400));
    let (src, _) = opened(&store);
    let header = src.chat(CONV).expect("header");
    assert_eq!(
        (header.timer.as_str(), header.timer_secs),
        ("1 day", 86_400)
    );
    let offered: Vec<(u32, &str)> = header
        .timers
        .iter()
        .map(|t| (t.secs, t.label.as_str()))
        .collect();
    assert_eq!(
        offered,
        [
            (0, "Off"),
            (30, "30 seconds"),
            (3600, "1 hour"),
            (86_400, "1 day"),
            (604_800, "1 week")
        ]
    );

    // No timer set, none offered: no mark and no menu.
    let (src, _) = opened(&Store::new(&[(1, 3)]));
    let header = src.chat(CONV).expect("header");
    assert!(header.timer.is_empty() && header.timer_secs == 0 && header.timers.is_empty());

    // Where we cannot send, the timer shows but cannot be changed.
    store.features.cannot_send = Some(CannotSend::NotMember);
    let (src, _) = opened(&store);
    let header = src.chat(CONV).expect("header");
    assert_eq!(header.timer, "1 day");
    assert!(header.timers.is_empty());
}

#[test]
fn action_flags_are_read_again_when_a_window_ends() {
    let mut store = store(6);
    // The edit window of the newest own message ends 60 ms after the page was read.
    store.convs.get_mut(&1).expect("conv")[5].actions.until = Some(NOW + 60);
    let before = Instant::now();
    let (mut src, sent) = opened(&store);
    let deadline = src.flags_deadline().expect("a deadline");
    assert!(
        deadline > before + Duration::from_millis(60),
        "not before the window ends"
    );
    assert!(
        deadline
            < Instant::now() + Duration::from_millis(60) + FLAGS_SLACK + Duration::from_millis(5)
    );
    // Not due yet: nothing is read.
    src.refresh_flags();
    assert!(take_queries(&sent).is_empty());
    std::thread::sleep(
        deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(10),
    );
    src.refresh_flags();
    let queries = take_queries(&sent);
    assert!(
        matches!(&queries[..], [(_, Query::Timeline(_))]),
        "one re-read"
    );
    assert_eq!(src.flags_deadline(), None, "armed again by the answer");
    // The core now says the window is over: nothing ends any more.
    store.convs.get_mut(&1).expect("conv")[5].actions = MessageActions {
        edit: false,
        until: None,
        ..own_actions()
    };
    for (t, q) in queries {
        src.on_event(&result(t, store.answer(&q)));
    }
    assert_eq!(src.flags_deadline(), None);
    assert!(!row(&src, id(6)).actions.edit);
    // A closed conversation has no deadline.
    let (mut src, _) = opened(&store_with_until());
    assert!(src.flags_deadline().is_some());
    src.open_conversation(None);
    assert_eq!(src.flags_deadline(), None);
}

fn store_with_until() -> Store {
    let mut s = store(3);
    s.convs.get_mut(&1).expect("conv")[2].actions.until = Some(NOW + 3_600_000);
    s
}

fn conn(src: &mut CoreSource, state: ConnState) -> Changes {
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state,
    })
}

#[test]
fn account_states_show_a_banner() {
    let (mut src, _) = opened(&store(3));
    // The queried row says Connecting.
    let b = src.banner(None).expect("connecting");
    assert_eq!(
        (b.kind, b.text.as_str()),
        (BannerKind::Connecting, "Connecting…")
    );
    assert!(b.kind.passing() && b.action.is_empty() && b.url.is_empty());

    assert!(conn(&mut src, ConnState::Online { detail: None }).accounts);
    assert!(src.banner(None).is_none(), "nothing to say when online");

    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    let b = src.banner(None).expect("offline");
    assert_eq!(b.kind, BannerKind::Offline);
    assert_eq!(
        b.text,
        "Offline. Messages are sent once the connection is back."
    );

    conn(
        &mut src,
        ConnState::Syncing {
            fraction: Some(0.4),
        },
    );
    assert_eq!(src.banner(None).map(|b| b.kind), Some(BannerKind::Syncing));

    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::SolveChallenge {
            url: "https://challenge.invalid/x".into(),
        }),
    );
    let b = src.banner(None).expect("challenge");
    assert_eq!(
        (b.kind, b.action.as_str(), b.url.as_str()),
        (
            BannerKind::Challenge,
            "Verify…",
            "https://challenge.invalid/x"
        )
    );
    assert!(!b.kind.passing());
    assert!(
        !format!("{b:?}").contains("challenge.invalid"),
        "no URL in logs"
    );
    // Only a web address is handed to the browser: whatever else a backend passes on (a
    // local file, another application's scheme, a plain-HTTP page) is dropped, and the
    // banner still offers the dialog for an answer pasted by hand.
    for url in [
        "file:///home/user/.local/share/applications/evil.desktop",
        "http://challenge.invalid/x",
        "smb://host/share",
        "javascript:alert(1)",
        "https://",
        "https://challenge.invalid/x y",
        "https://challenge.invalid/\nfile:///etc/passwd",
        "",
    ] {
        conn(
            &mut src,
            ConnState::NeedsUserAction(UserAction::SolveChallenge { url: url.into() }),
        );
        let b = src.banner(None).expect("challenge");
        assert_eq!(
            (b.kind, b.action.as_str(), b.url.as_str()),
            (BannerKind::Challenge, "Verify…", ""),
            "{url:?}"
        );
    }
    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::SolveChallenge {
            url: "HTTPS://signalcaptchas.org/challenge/generate.html".into(),
        }),
    );
    assert_eq!(
        src.banner(None).expect("challenge").url,
        "HTTPS://signalcaptchas.org/challenge/generate.html"
    );

    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::FixClock {
            skew_ms: 2 * 3_600_000,
        }),
    );
    let b = src.banner(None).expect("clock");
    assert_eq!(b.kind, BannerKind::Clock);
    assert_eq!(
        b.text,
        "The clock of this computer is about 2 hours ahead. Set the date and time to send and receive messages."
    );
    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: -240_000 }),
    );
    assert!(
        src.banner(None)
            .expect("clock")
            .text
            .contains("about 4 minutes behind")
    );

    // Unlinked: the banner offers to link again, and the composer says why it cannot send.
    let ch = conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    assert!(
        ch.accounts && ch.header,
        "the open chat's composer changes too"
    );
    let b = src.banner(None).expect("logged out");
    assert_eq!(
        (b.kind, b.action.as_str()),
        (BannerKind::LoggedOut, "Link Again")
    );
    assert_eq!(b.account, AccountId(1));
    let header = src.chat(CONV).expect("header");
    assert_eq!(
        header.cannot_send,
        "You can read this chat. To write here, link this device again."
    );
    assert!(header.timers.is_empty());

    // Only the accounts shown count.
    assert!(src.banner(Some(AccountId(9))).is_none());
    assert!(src.banner(Some(AccountId(1))).is_some());

    // An account the supervisor gave up on offers a restart.
    conn(&mut src, ConnState::Online { detail: None });
    src.on_event(&UiEvent::AccountError {
        account: AccountId(1),
        detail: "boom".into(),
        restartable: true,
    });
    let b = src.banner(None).expect("stopped");
    assert_eq!((b.kind, b.action.as_str()), (BannerKind::Error, "Restart"));
    assert!(src.restart_account(AccountId(1)));
    assert!(src.banner(None).is_none());
}

#[test]
fn a_relinked_account_frees_the_open_chat_whichever_answer_comes_first() {
    let store = store(3);
    let (mut src, sent) = opened(&store);
    let gone = kourier_core::facade::AccountRow {
        state: AccountState::LoggedOut {
            reason: "unlinked".into(),
        },
        ..account()
    };
    // Answers everything the source asked, the account list last (the queries run on two
    // connections: any order happens). Returns what the answer about the accounts changed.
    let answer_accounts_last = |src: &mut CoreSource, accounts: Vec<CoreAccount>| {
        let (about_accounts, others): (Vec<_>, Vec<_>) = take_queries(&sent)
            .into_iter()
            .partition(|(_, q)| matches!(q, Query::Accounts));
        assert_eq!(about_accounts.len(), 1, "one question about the accounts");
        for (token, query) in others {
            src.on_event(&result(token, store.answer(&query)));
        }
        let (token, _) = about_accounts[0];
        src.on_event(&result(token, QueryData::Accounts(accounts)))
    };

    // The account is logged out: its chat says why it cannot be written to.
    src.on_event(&UiEvent::AccountsChanged);
    let ch = answer_accounts_last(&mut src, vec![gone]);
    assert!(ch.header, "the open chat's composer follows its account");
    let header = src.chat(CONV).expect("header");
    assert_eq!(
        header.cannot_send,
        "You can read this chat. To write here, link this device again."
    );
    assert!(!header.deletes_sync, "nothing is told to other devices");

    // Linked again. The chat's own answers arrive first and are still read against the
    // logged-out account; the answer about the accounts must tell the header to be read
    // again, or the composer stays closed although the account is back.
    src.on_event(&UiEvent::AccountsChanged);
    let ch = answer_accounts_last(&mut src, vec![account()]);
    assert!(ch.accounts);
    assert!(
        ch.header,
        "the account's answer came last: the header is read again"
    );
    assert_eq!(src.chat(CONV).expect("header").cannot_send, "");

    // Without an open chat there is no header to read.
    src.open_conversation(None);
    src.on_event(&UiEvent::AccountsChanged);
    let ch = answer_accounts_last(&mut src, vec![account()]);
    assert!(ch.accounts && !ch.header);
}

#[test]
fn the_most_pressing_account_state_is_shown_with_its_account() {
    let (mut src, sent) = source();
    src.reload();
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        conn: ConnState::Offline { retry_at_ms: None },
        ..account()
    };
    let gone = kourier_core::facade::AccountRow {
        id: AccountId(3),
        uuid: AccountUuid::new_random(),
        display_name: None,
        state: AccountState::LoggedOut {
            reason: "unlinked".into(),
        },
        conn: ConnState::Offline { retry_at_ms: None },
        ..account()
    };
    let store = Store::new(&[(1, 2)]);
    for (t, q) in take_queries(&sent) {
        let data = match q {
            Query::Accounts => QueryData::Accounts(vec![
                kourier_core::facade::AccountRow {
                    conn: ConnState::Online { detail: None },
                    ..account()
                },
                work.clone(),
                gone.clone(),
            ]),
            other => store.answer(&other),
        };
        src.on_event(&result(t, data));
    }
    // Logged out beats offline; with several accounts the banner names its account.
    let b = src.banner(None).expect("banner");
    assert_eq!((b.account, b.kind), (AccountId(3), BannerKind::LoggedOut));
    assert!(
        b.text
            .starts_with("Test network: This device is no longer linked.")
    );
    let b = src.banner(Some(AccountId(2))).expect("work");
    assert_eq!(b.kind, BannerKind::Offline);
    assert!(b.text.starts_with("Work: Offline."));
    assert!(src.banner(Some(AccountId(1))).is_none());

    // An answer that comes back from the browser goes to the account that waits for one,
    // also while the banner is about another account's more pressing state.
    assert_eq!(src.challenged_account(), None);
    src.on_event(&UiEvent::ConnState {
        account: AccountId(2),
        state: ConnState::NeedsUserAction(UserAction::SolveChallenge {
            url: "https://challenge.invalid/x".into(),
        }),
    });
    assert_eq!(
        src.banner(None).map(|b| (b.account, b.kind)),
        Some((AccountId(3), BannerKind::LoggedOut))
    );
    assert_eq!(src.challenged_account(), Some(AccountId(2)));
    src.on_event(&UiEvent::ConnState {
        account: AccountId(2),
        state: ConnState::Online { detail: None },
    });
    assert_eq!(src.challenged_account(), None);
}

#[test]
fn link_again_starts_the_login_of_the_same_account() {
    let (mut src, sent) = opened(&store(3));
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    commands(&sent);
    let view = src.relink(AccountId(1)).expect("relink");
    assert_eq!(view.kind, LoginViewKind::Waiting);
    assert!(matches!(
        &commands(&sent)[..],
        [UiCommand::Relink { account: AccountId(1), flow }] if flow == "qr"
    ));
    // The account exists already: its steps are followed from the first one.
    let ch = src.on_event(&UiEvent::LoginStep {
        account: AccountId(1),
        step: LoginStep {
            step_id: "done".into(),
            txn: 1,
            kind: LoginStepKind::Done,
        },
    });
    assert!(ch.login);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    let queries: Vec<Query> = take_queries(&sent).into_iter().map(|(_, q)| q).collect();
    assert!(
        queries.contains(&Query::Accounts),
        "the account row is read again"
    );

    // A refused relink shows on the link page.
    let _ = src.relink(AccountId(1)).expect("again");
    let ch = src.on_action_failed(
        "Relink",
        &CoreError::InvalidState("the account is linked"),
        &ActionSubject::None,
    );
    assert!(ch.login);
    assert_eq!(
        src.login_view().map(|v| (v.kind, v.error)),
        Some((
            LoginViewKind::Failed,
            "This account could not be linked again.".to_owned()
        ))
    );
}

#[test]
fn try_again_after_a_failed_relink_links_the_same_account() {
    let (mut src, sent) = opened(&store(3));
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    let _ = src.relink(AccountId(1)).expect("relink");
    // The code ran out: the link page shows the failure with "Try again".
    let ch = src.on_event(&UiEvent::LoginStep {
        account: AccountId(1),
        step: LoginStep {
            step_id: "failed".into(),
            txn: 1,
            kind: LoginStepKind::Failed {
                error: kourier_model::BackendError::new(
                    kourier_model::ErrorKind::Expired,
                    "expired",
                ),
            },
        },
    });
    assert!(ch.login);
    let failed = src.login_view().expect("view");
    assert_eq!(failed.kind, LoginViewKind::Failed);
    assert!(failed.retry, "the link page offers to try again");
    commands(&sent);
    // "Try again" (the link page starts a login on the same network): the account that was
    // being linked again, with its history, not a second account.
    let view = src.start_login(NetworkKind::Fake).expect("try again");
    assert_eq!(view.kind, LoginViewKind::Waiting);
    let cmds = commands(&sent);
    assert!(
        matches!(&cmds[..], [UiCommand::Relink { account: AccountId(1), flow }] if flow == "qr"),
        "{cmds:?}"
    );
    // Its steps are followed again.
    let ch = src.on_event(&UiEvent::LoginStep {
        account: AccountId(1),
        step: LoginStep {
            step_id: "done".into(),
            txn: 2,
            kind: LoginStepKind::Done,
        },
    });
    assert!(ch.login);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    commands(&sent);
    // A finished relink is over: the next login on the network adds an account.
    let _ = src.start_login(NetworkKind::Fake).expect("add");
    let cmds = commands(&sent);
    assert!(
        matches!(
            &cmds[..],
            [UiCommand::AddAccount {
                network: NetworkKind::Fake,
                ..
            }]
        ),
        "{cmds:?}"
    );

    // A failed relink the user walked away from is forgotten as well: "Add account" adds one.
    let (mut src, sent) = opened(&store(3));
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    let _ = src.relink(AccountId(1)).expect("relink");
    src.on_action_failed(
        "Relink",
        &CoreError::InvalidState("the account is linked"),
        &ActionSubject::None,
    );
    src.cancel_login();
    commands(&sent);
    let _ = src.start_login(NetworkKind::Fake).expect("add");
    let cmds = commands(&sent);
    assert!(
        matches!(
            &cmds[..],
            [UiCommand::AddAccount {
                network: NetworkKind::Fake,
                ..
            }]
        ),
        "{cmds:?}"
    );
    // And a relink still in progress is not a retry: choosing the network adds an account
    // (the relink is cancelled first).
    let _ = src.relink(AccountId(1)).expect("relink");
    commands(&sent);
    let _ = src.start_login(NetworkKind::Fake).expect("add");
    let cmds = commands(&sent);
    assert!(
        matches!(
            &cmds[..],
            [
                UiCommand::CancelLogin {
                    account: AccountId(1)
                },
                UiCommand::AddAccount {
                    network: NetworkKind::Fake,
                    ..
                }
            ]
        ),
        "{cmds:?}"
    );
}

#[test]
fn a_link_or_unlink_rereads_what_the_open_chat_offers() {
    let (mut src, sent) = opened(&store(3));
    take_queries(&sent);
    src.on_event(&UiEvent::AccountsChanged);
    let queries: Vec<Query> = take_queries(&sent).into_iter().map(|(_, q)| q).collect();
    assert!(queries.contains(&Query::Accounts));
    assert!(
        queries.contains(&Query::Conversation(CONV)),
        "the composer's state"
    );
    assert!(
        queries.iter().any(|q| matches!(q, Query::Timeline(_))),
        "the rows' action flags"
    );
}

#[test]
fn own_and_authors_pictures_come_from_the_core() {
    struct Urls;
    impl AvatarUrls for Urls {
        fn url(&self, _account: &AccountUuid, avatar: &BlobRef) -> Option<String> {
            Some(format!("image://kourier/avatar/{}", avatar.path.display()))
        }
    }
    let blob = |path: &str| BlobRef {
        path: path.into(),
        key: BlobKey::new([1; 32]),
    };
    let mut store = store(4);
    for m in store.convs.get_mut(&1).expect("conv") {
        if m.author.is_some() {
            m.author_avatar = Some(blob("author"));
        }
    }
    let (mut src, sent) = source();
    src.set_avatar_urls(Box::new(Urls));
    src.reload();
    for (t, q) in take_queries(&sent) {
        let data = match q {
            Query::Accounts => QueryData::Accounts(vec![kourier_core::facade::AccountRow {
                avatar: Some(blob("me")),
                ..account()
            }]),
            other => store.answer(&other),
        };
        src.on_event(&result(t, data));
    }
    // The drawer's entry of our account shows our own picture.
    assert_eq!(src.accounts()[0].avatar_url, "image://kourier/avatar/me");
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    let rows = src.timeline(CONV);
    let shown: Vec<&MessageRow> = rows.iter().filter(|r| r.show_avatar).collect();
    assert!(!shown.is_empty());
    assert!(
        shown
            .iter()
            .all(|r| r.avatar_url == "image://kourier/avatar/author"),
        "the author's own picture, without a direct chat with them"
    );
}

#[test]
fn a_custom_timer_is_offered_within_the_range_the_chat_states() {
    use kourier_model::TimerRange;
    let mut store = store(3);
    store.features.disappearing =
        BTreeMap::from([(DisappearKind::AfterRead, vec![30, 3600, 86_400])]);
    // No range stated: the menu is all there is.
    let (src, _) = opened(&store);
    assert_eq!(src.chat(CONV).expect("header").timer_range, None);
    store.features.disappearing_custom = BTreeMap::from([(
        DisappearKind::AfterRead,
        TimerRange {
            min_secs: 1,
            max_secs: 2_419_200,
            step_secs: 0,
        },
    )]);
    // A timer that is none of the offered values (set here as a custom time, or on the
    // phone) is listed in its place by length, so the menu marks what holds.
    store.timer = Some(Duration::from_secs(45));
    let (mut src, sent) = opened(&store);
    let header = src.chat(CONV).expect("header");
    let span = header.timer_range.expect("a custom range");
    assert_eq!((span.min, span.max, span.step), (1, 2_419_200, 1));
    assert!(span.contains(45) && span.contains(2_419_200));
    assert!(!span.contains(0) && !span.contains(2_419_201));
    let offered: Vec<(u32, &str)> = header
        .timers
        .iter()
        .map(|t| (t.secs, t.label.as_str()))
        .collect();
    assert_eq!(
        offered,
        [
            (0, "Off"),
            (30, "30 seconds"),
            (45, "45 seconds"),
            (3600, "1 hour"),
            (86_400, "1 day")
        ]
    );
    assert_eq!(header.timer, "45 seconds");
    // Any value of the range goes to the core as it is.
    sent.borrow_mut().clear();
    assert!(src.set_timer(CONV, 5400));
    assert!(matches!(
        commands(&sent).as_slice(),
        [UiCommand::SetConversationTimer { conv: CONV, timer: Some(t) }] if t.as_secs() == 5400
    ));
    // A range with steps: what an input settles on.
    let stepped = crate::conversations::TimerSpan {
        min: 60,
        max: 3600,
        step: 60,
    };
    assert!(stepped.contains(120) && !stepped.contains(90) && !stepped.contains(30));
    assert_eq!(stepped.nearest(89), 60);
    assert_eq!(stepped.nearest(91), 120);
    assert_eq!(stepped.nearest(1), 60);
    assert_eq!(stepped.nearest(999_999), 3600);
    // A span that makes no sense (its end before its start, no step) has one value and
    // never panics: the fields are public and a network states them.
    let odd = crate::conversations::TimerSpan {
        min: 90,
        max: 30,
        step: 0,
    };
    assert_eq!(odd.nearest(0), 90);
    assert_eq!(odd.nearest(10_000), 90);
    assert!(!odd.contains(90) && !odd.contains(30));
    let stepless = crate::conversations::TimerSpan {
        min: 5,
        max: 10,
        step: 0,
    };
    assert!(stepless.contains(7));
    assert_eq!(stepless.nearest(7), 7);
    // Only a range, no offered values: "Off" and what is set.
    store.features.disappearing = BTreeMap::new();
    let (src, _) = opened(&store);
    let header = src.chat(CONV).expect("header");
    let offered: Vec<u32> = header.timers.iter().map(|t| t.secs).collect();
    assert_eq!(offered, [0, 45]);
    // Where we cannot send, neither the menu nor the range is offered.
    store.features.cannot_send = Some(CannotSend::NotMember);
    let (src, _) = opened(&store);
    let header = src.chat(CONV).expect("header");
    assert!(header.timers.is_empty() && header.timer_range.is_none());
}

#[test]
fn clearing_and_deleting_send_their_commands_and_say_what_failed() {
    use kourier_core::facade::Refusal;
    let mut store = store(3);
    store.features.delete_for_me = true;
    let (mut src, sent) = opened(&store);
    // The header says whether the account's other devices follow a delete made here.
    assert!(src.chat(CONV).expect("header").deletes_sync);
    src.save_draft(CONV, "half a thought");
    sent.borrow_mut().clear();
    assert!(src.clear_conversation(CONV));
    assert!(src.delete_attachment(AttachmentId(7)));
    assert!(src.delete_conversation(CONV));
    let sent_now = commands(&sent);
    assert!(matches!(
        sent_now.as_slice(),
        [
            UiCommand::ClearConversation { conv: CONV },
            UiCommand::DeleteAttachment {
                attachment: AttachmentId(7)
            },
            UiCommand::DeleteConversation { conv: CONV },
        ]
    ));
    // The draft of a deleted chat is not shown for it any more.
    assert_eq!(src.draft(CONV), "");

    // The core's reasons for not deleting an attachment, each in its own words.
    let refused = |reason| CoreError::Refused {
        reason,
        detail: "the core's own words",
    };
    let attachment = ActionSubject::Attachment(AttachmentId(7));
    for (reason, text) in [
        (
            Refusal::BeingSent,
            "This message is being sent. Delete the attachment once it is.",
        ),
        (Refusal::Gone, "This attachment is no longer there."),
        (Refusal::Deleted, "This attachment is no longer there."),
        (Refusal::WrongKind, "The attachment could not be deleted."),
    ] {
        let ch = src.on_action_failed("DeleteAttachment", &refused(reason), &attachment);
        assert!(ch.notice);
        assert_eq!(src.notice(), text, "{reason:?}");
    }
    let conv = ActionSubject::Conversation(CONV);
    src.on_action_failed("ClearConversation", &CoreError::Stopped, &conv);
    assert_eq!(src.notice(), "The chat history could not be deleted.");
    src.on_action_failed("DeleteConversation", &CoreError::Stopped, &conv);
    assert_eq!(src.notice(), "The chat could not be deleted.");

    // A network without such a message, or a logged-out account: the delete stays here.
    store.features.delete_for_me = false;
    let (src, _) = opened(&store);
    assert!(!src.chat(CONV).expect("header").deletes_sync);
    store.features.delete_for_me = true;
    let (mut src, _) = opened(&store);
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    assert!(!src.chat(CONV).expect("header").deletes_sync);
}

#[test]
fn an_edit_the_core_refuses_is_explained_by_its_reason() {
    use kourier_core::facade::Refusal;
    let (mut src, sent) = opened(&store(6));
    let refused = |reason| CoreError::Refused {
        reason,
        detail: "the core's own words",
    };
    let subject = ActionSubject::Message(id(3));
    // The row still offers the edit in every case: only the core's reason tells them apart.
    assert!(row(&src, id(3)).actions.edit);
    for (reason, text) in [
        (Refusal::TooOld, "This message can no longer be edited."),
        (
            Refusal::EditedTooOften,
            "This message was edited too often to change it again.",
        ),
        (Refusal::Deleted, "This message was deleted."),
        (
            Refusal::BeingSent,
            "This message is being sent. Edit it once it is.",
        ),
        (
            Refusal::TooLong,
            "This text is too long for one message. Shorten it to save the edit.",
        ),
    ] {
        let ch = src.on_action_failed("EditMessage", &refused(reason), &subject);
        assert!(ch.notice, "{reason:?}");
        assert_eq!(src.notice(), text, "{reason:?}");
    }
    // The text is what it was: nothing to tell the user; the rows are read again.
    src.clear_notice();
    sent.borrow_mut().clear();
    let ch = src.on_action_failed("EditMessage", &refused(Refusal::Unchanged), &subject);
    assert!(!ch.notice);
    assert_eq!(src.notice(), "");
    assert!(
        take_queries(&sent)
            .iter()
            .any(|(_, q)| matches!(q, Query::Timeline(_)))
    );
}

#[test]
fn a_lone_document_is_called_by_its_name_in_the_list() {
    let names = NameMap::new();
    let show = |p: &Preview| {
        let s = core_preview(p, &names);
        (s.text, s.icon)
    };
    let media = |kind, count, name: Option<&str>| Preview {
        attachment: Some(kind),
        attachments: count,
        file_name: name.map(str::to_owned),
        ..preview("Media")
    };
    let file = core_preview(&media(AttachmentKind::File, 1, None), &names);
    // Its name, with the icon of what it is.
    assert_eq!(
        show(&media(AttachmentKind::File, 1, Some("Invoice 2026-10.pdf"))),
        ("Invoice 2026-10.pdf".to_owned(), file.icon.clone())
    );
    // A name from the sender is one line, whatever it holds.
    assert_eq!(
        show(&media(AttachmentKind::File, 1, Some("a\nb.pdf"))).0,
        "a b.pdf"
    );
    // A sound file too; a blank name says nothing, so the kind is said.
    assert_eq!(
        show(&media(AttachmentKind::Audio, 1, Some("Interview.mp3"))).0,
        "Interview.mp3"
    );
    assert_eq!(
        show(&media(AttachmentKind::File, 1, Some("  "))).0,
        file.text
    );
    assert_eq!(show(&media(AttachmentKind::File, 1, None)).0, file.text);
    // Pictures, videos and voice notes are what they are, and several files are counted.
    let photo = show(&media(AttachmentKind::Image, 1, Some("IMG_0001.jpg")));
    assert_eq!(photo.0, "Photo");
    assert_eq!(
        show(&media(AttachmentKind::Voice, 1, Some("voice.ogg"))).0,
        "Voice message"
    );
    let several = show(&media(AttachmentKind::File, 3, Some("first.pdf")));
    assert!(!several.0.contains("first.pdf"), "{}", several.0);
    // A caption still says more than the name.
    let captioned = Preview {
        text: Some("The signed copy".into()),
        ..media(AttachmentKind::File, 1, Some("Contract.pdf"))
    };
    assert_eq!(show(&captioned), ("The signed copy".to_owned(), file.icon));
}

#[test]
fn a_chat_the_user_deleted_stays_out_of_the_list() {
    use kourier_core::facade::ConversationPage;
    let store = Store::new(&[(1, 3), (2, 3), (3, 3)]);
    let (mut src, sent) = source();
    src.reload();
    // The list as the core answers it, with one row flagged hidden (a core that listed
    // such a row would be wrong; the list still never shows it).
    for (token, query) in take_queries(&sent) {
        let answer = match (&query, store.answer(&query)) {
            (Query::Conversations(_), QueryData::Conversations(mut page)) => {
                page.rows[1].hidden = true;
                QueryData::Conversations(ConversationPage {
                    rows: page.rows,
                    next: None,
                })
            }
            (_, other) => other,
        };
        src.on_event(&result(token, answer));
    }
    let shown: Vec<i64> = src
        .conversations(&ConversationFilter::default())
        .iter()
        .map(|c| c.id.0)
        .collect();
    assert_eq!(shown.len(), 2);
    assert!(!shown.contains(&2), "{shown:?}");
    // Its unread messages are not counted on the badge either.
    assert_eq!(src.unread_total(), 0);
}

#[test]
fn the_account_row_shows_its_number() {
    let (mut src, sent) = source();
    src.reload();
    for (token, query) in take_queries(&sent) {
        let answer = match &query {
            Query::Accounts => QueryData::Accounts(vec![kourier_core::facade::AccountRow {
                number: Some("+380 44 123\n4567".into()),
                ..account()
            }]),
            other => Store::new(&[(1, 1)]).answer(other),
        };
        src.on_event(&result(token, answer));
    }
    let rows = src.accounts();
    assert_eq!(rows.len(), 1);
    // One line, as the network reported it.
    assert_eq!(rows[0].number, "+380 44 123 4567");
    // Never in a log line.
    assert!(!format!("{:?}", rows[0]).contains("380"));
    // An account without one (a network that has none, or not reported yet).
    let (src, _) = opened(&store(1));
    assert_eq!(src.accounts()[0].number, "");
}

#[test]
fn the_composer_learns_what_the_chats_network_says_about_files() {
    let mut store = store(3);
    let (src, _) = opened(&store);
    assert_eq!(src.strip_rules(CONV), crate::files::StripRules::default());
    assert_eq!(src.max_text_bytes(CONV), None);
    store.features.hard_to_save = ["exe".to_owned(), "apk".to_owned()].into();
    store.features.max_text_bytes = Some(65_536);
    let (src, _) = opened(&store);
    let rules = src.strip_rules(CONV);
    assert!(rules.hard_to_save.contains("apk") && rules.hard_to_save.contains("exe"));
    assert_eq!(rules.network, "Test network");
    assert_eq!(src.max_text_bytes(CONV), Some(65_536));
    // Only for the chat whose features are loaded.
    assert_eq!(
        src.strip_rules(ConversationId(2)),
        crate::files::StripRules::default()
    );
    assert_eq!(src.max_text_bytes(ConversationId(2)), None);
}

/// Link feedback L3: a chat whose imported history still arrives (more batches announced),
/// or whose account's history transfer still runs, is neither cleared nor deleted: a
/// cleared chat would fill again. The menu says why; afterwards both work again.
#[test]
fn a_chat_still_filling_is_not_cleared_or_deleted_and_says_why() {
    use kourier_model::{HistoryTransfer, HistoryTransferFailure};
    let account = AccountId(1);
    let (mut src, sent) = opened(&store(3));
    assert_eq!(src.chat(CONV).expect("header").still_filling, "");
    let progress = |has_more| UiEvent::HistoryProgress {
        account,
        conv: CONV,
        progress: None,
        has_more,
    };
    let ch = src.on_event(&progress(true));
    assert!(ch.header, "the open chat's menu changes");
    let why = src.chat(CONV).expect("header").still_filling;
    assert_eq!(why, "Available once this chat's history has arrived");
    sent.borrow_mut().clear();
    assert!(!src.clear_conversation(CONV));
    assert!(!src.delete_conversation(CONV));
    assert!(commands(&sent).is_empty(), "nothing sent");
    // Another chat is not held back.
    assert!(
        !src.on_event(&UiEvent::HistoryProgress {
            account,
            conv: ConversationId(2),
            progress: None,
            has_more: false,
        })
        .header
    );
    // The last batch: both work again.
    assert!(src.on_event(&progress(false)).header);
    assert_eq!(src.chat(CONV).expect("header").still_filling, "");
    assert!(src.clear_conversation(CONV));

    // While the account's transfer runs, every chat of it waits, and sending stays allowed.
    let ch = src.on_event(&UiEvent::HistoryTransfer {
        account,
        state: HistoryTransfer::Reading,
    });
    assert!(ch.header);
    let header = src.chat(CONV).expect("header");
    assert!(!header.still_filling.is_empty());
    assert_eq!(header.cannot_send, "", "a 1:1 chat can be written to");
    assert!(!src.delete_conversation(CONV));
    // A failed import leaves its chat as it got it: nothing waits any more.
    src.on_event(&progress(true));
    src.on_event(&UiEvent::HistoryTransfer {
        account,
        state: HistoryTransfer::Failed(HistoryTransferFailure::Incomplete),
    });
    assert_eq!(src.chat(CONV).expect("header").still_filling, "");
    assert!(src.delete_conversation(CONV));

    // A logout stops the delivery: nothing waits.
    let (mut src, _) = opened(&store(3));
    src.on_event(&progress(true));
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    assert_eq!(src.chat(CONV).expect("header").still_filling, "");
}
