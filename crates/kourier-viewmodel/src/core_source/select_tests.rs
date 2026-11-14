//! `CoreSource`: Copy as Text of the selection mode against the simulated core of
//! [`super::tests`]: the by-id query, and the text built from the rows the timeline maps.

use kourier_core::facade::{CoreError, Query, QueryData, TimelinePage, UiEvent};
use kourier_model::{ConversationId, MessageId};

use super::tests::{NOW, Store, message, result, settle, source, take_queries};
use super::*;
use crate::source::UiSource;

#[test]
fn copy_as_text_asks_the_core_for_the_messages_and_copies_their_text() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 5)]);
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let ids = vec![MessageId(100_003), MessageId(100_001)];
    let ch = src.copy_selected(ConversationId(1), ids.clone());
    assert!(!ch.copy, "the answer comes with the core's");
    let queries = take_queries(&sent);
    assert_eq!(queries.len(), 1, "{queries:?}");
    let (token, query) = &queries[0];
    assert_eq!(token.model, MODEL_COPY);
    let Query::MessagesByIds { conv, ids: asked } = query else {
        panic!("{query:?}");
    };
    assert_eq!((*conv, asked), (ConversationId(1), &ids));
    // The core answers oldest first.
    let page = TimelinePage {
        conv: ConversationId(1),
        rows: vec![message(1, 1, false), message(1, 3, false)],
        has_older: false,
        has_newer: false,
        now: NOW,
    };
    let ch = src.on_event(&result(*token, QueryData::MessagesByIds(page)));
    assert!(ch.copy);
    let answer = src.take_copy().expect("an answer");
    let text = answer.text.expect("a text");
    let lines: Vec<&str> = text.expose().lines().collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].ends_with("Ari Arden: 1:1"), "{lines:?}");
    assert!(lines[1].ends_with("You: 1:3"), "{lines:?}");
    assert!(
        lines[0].starts_with("[") && lines[0].contains("] "),
        "{lines:?}"
    );
    assert!(src.take_copy().is_none(), "taken once");
}

#[test]
fn a_failed_read_copies_nothing_and_says_so() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 5)]);
    src.reload();
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    src.copy_selected(ConversationId(1), vec![MessageId(100_001)]);
    let (token, _) = take_queries(&sent).remove(0);
    let ch = src.on_event(&UiEvent::QueryResult {
        token,
        result: Err(std::sync::Arc::new(CoreError::Stopped)),
    });
    assert!(ch.copy);
    let answer = src.take_copy().expect("an answer");
    assert!(answer.text.is_none());
    assert!(!answer.told.is_empty());
}
