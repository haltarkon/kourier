//! `CoreSource`: the saved RegExp filters of the settings page (R2), against the simulated
//! core of [`super::tests`].

use std::sync::Arc;

use kourier_core::facade::filters::{Field, PatternError, PatternErrorKind};
use kourier_core::facade::{Filter, FilterId, FilterList, FilterRefusal, FilterSet, Query};

use super::tests::{Store, result, source, take_queries};
use super::*;
use crate::filters::{SWATCHES, spec_of};

fn list(names: &[&str], revision: u64) -> FilterList {
    let filters: Vec<Filter> = names
        .iter()
        .enumerate()
        .map(|(i, n)| Filter {
            id: FilterId(i as i64 + 1),
            spec: spec_of(
                n,
                "word",
                "",
                "",
                false,
                false,
                false,
                true,
                SWATCHES[i % 8],
            ),
        })
        .collect();
    FilterList {
        set: Arc::new(FilterSet::build(&filters)),
        filters,
        revision,
    }
}

fn filter_queries(sent: &super::tests::Sent) -> Vec<QueryToken> {
    take_queries(sent)
        .into_iter()
        .filter(|(_, q)| matches!(q, Query::Filters))
        .map(|(t, _)| t)
        .collect()
}

fn others(sent: &super::tests::Sent) -> Vec<UiCommand> {
    let all = std::mem::take(&mut *sent.borrow_mut());
    let (queries, rest): (Vec<UiCommand>, Vec<UiCommand>) = all
        .into_iter()
        .partition(|c| matches!(c, UiCommand::Query { .. }));
    *sent.borrow_mut() = queries;
    rest
}

#[test]
fn the_filters_are_read_when_the_page_asks_and_again_on_every_change() {
    let (mut src, sent) = source();
    assert!(src.filter_rows().is_none(), "unknown before the core said");
    take_queries(&sent);
    src.load_filters();
    let asked = filter_queries(&sent);
    assert_eq!(asked.len(), 1);
    let ch = src.on_event(&result(asked[0], QueryData::Filters(list(&["A", "B"], 1))));
    assert!(ch.filters);
    let rows = src.filter_rows().expect("known");
    assert_eq!(rows.len(), 2);
    assert!(rows[0].first && rows[1].last);
    // A change of the list is read again, and a new answer with a new revision shows.
    take_queries(&sent);
    src.on_event(&UiEvent::FiltersChanged { revision: 2 });
    let asked = filter_queries(&sent);
    assert_eq!(asked.len(), 1, "read again after FiltersChanged");
    let ch = src.on_event(&result(asked[0], QueryData::Filters(list(&["B"], 2))));
    assert!(ch.filters);
    assert_eq!(src.filter_rows().expect("known").len(), 1);
    // The same revision again changes nothing.
    take_queries(&sent);
    src.on_event(&UiEvent::FiltersChanged { revision: 2 });
    let asked = filter_queries(&sent);
    let ch = src.on_event(&result(asked[0], QueryData::Filters(list(&["B"], 2))));
    assert!(!ch.filters);
}

#[test]
fn a_save_a_delete_and_a_move_reach_the_core() {
    let (mut src, sent) = source();
    others(&sent);
    let spec = spec_of("N", "x", "", "", false, true, false, true, SWATCHES[0]);
    assert!(src.save_filter(None, spec.clone()));
    assert!(src.save_filter(Some(FilterId(3)), spec));
    assert!(src.delete_filter(FilterId(3)));
    assert!(src.move_filter(FilterId(3), 0));
    let cmds = others(&sent);
    assert!(matches!(
        cmds.as_slice(),
        [
            UiCommand::SaveFilter { id: None, .. },
            UiCommand::SaveFilter {
                id: Some(FilterId(3)),
                ..
            },
            UiCommand::DeleteFilter { id: FilterId(3) },
            UiCommand::MoveFilter {
                id: FilterId(3),
                to: 0
            },
        ]
    ));
}

#[test]
fn a_refused_save_is_worded_under_its_field_and_taken_once() {
    let (mut src, _sent) = source();
    let refusal = FilterRefusal::Pattern(
        Field::Chat,
        PatternError {
            kind: PatternErrorKind::UnclosedGroup,
            at_utf16: 4,
        },
    );
    let ch = src.on_event(&UiEvent::ActionFailed {
        account: None,
        action: "SaveFilter",
        subject: ActionSubject::None,
        error: Arc::new(CoreError::Filter(refusal)),
    });
    assert!(ch.filters);
    assert!(!ch.notice, "the dialog says it, not a toast");
    let note = src.take_filter_refusal().expect("a refusal waits");
    assert_eq!(note.field, "chat");
    assert!(note.text.contains("at character 5"), "{}", note.text);
    assert!(src.take_filter_refusal().is_none());
    // A failure that is no refusal is worded as not saved, under no input.
    src.on_event(&UiEvent::ActionFailed {
        account: None,
        action: "SaveFilter",
        subject: ActionSubject::None,
        error: Arc::new(CoreError::Stopped),
    });
    let note = src.take_filter_refusal().expect("a refusal waits");
    assert_eq!(note.field, "");
    assert_eq!(note.text, "The filter could not be saved.");
}

#[test]
fn a_refused_delete_reads_the_list_again_and_says_so() {
    let (mut src, sent) = source();
    take_queries(&sent);
    let ch = src.on_event(&UiEvent::ActionFailed {
        account: None,
        action: "DeleteFilter",
        subject: ActionSubject::None,
        error: Arc::new(CoreError::Filter(FilterRefusal::Unknown)),
    });
    assert!(ch.notice);
    assert_eq!(src.notice(), "The filter could not be changed.");
    assert_eq!(filter_queries(&sent).len(), 1);
}

#[test]
fn the_store_is_not_needed_for_the_empty_list() {
    // A source for a core that has no filters shows an empty list, not "unknown".
    let _ = Store::new(&[]);
    let (mut src, sent) = source();
    take_queries(&sent);
    src.load_filters();
    let asked = filter_queries(&sent);
    src.on_event(&result(asked[0], QueryData::Filters(list(&[], 1))));
    assert_eq!(src.filter_rows().map(|r| r.len()), Some(0));
}

/// A list with one Highlight filter whose text pattern is `pattern`.
fn marking_list(pattern: &str, revision: u64) -> FilterList {
    let filters: Vec<Filter> = if pattern.is_empty() {
        Vec::new()
    } else {
        vec![Filter {
            id: FilterId(1),
            spec: spec_of(
                "Marker",
                pattern,
                "",
                "",
                false,
                false,
                false,
                true,
                SWATCHES[2],
            ),
        }]
    };
    FilterList {
        set: Arc::new(FilterSet::build(&filters)),
        filters,
        revision,
    }
}

fn mark_counts(src: &CoreSource) -> Vec<usize> {
    src.timeline(ConversationId(1))
        .iter()
        .filter(|r| r.kind == crate::RowKind::Message)
        .filter_map(|r| match r.blocks.first() {
            Some(crate::timeline::Block::Text(t)) => Some(t.marks.len()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_rows_are_marked_from_the_compiled_set_and_marked_again_when_it_changes() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 4)]);
    src.reload();
    super::tests::settle(&mut src, &sent, &store);
    src.open_conversation(Some(ConversationId(1)));
    super::tests::settle(&mut src, &sent, &store);
    assert!(
        mark_counts(&src).iter().all(|n| *n == 0),
        "no filter: no mark"
    );

    // A set arrives: every text ("1:2") holds the pattern once; the colour is the filter's own.
    take_queries(&sent);
    src.load_filters();
    let asked = filter_queries(&sent);
    let ch = src.on_event(&result(asked[0], QueryData::Filters(marking_list(":", 2))));
    assert!(ch.filters && ch.timeline, "the open chat is marked again");
    let rows = src.timeline(ConversationId(1));
    let marks: Vec<_> = rows
        .iter()
        .filter_map(|r| match r.blocks.first() {
            Some(crate::timeline::Block::Text(t)) => Some(t.marks.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(marks.len(), 4);
    for m in &marks {
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].start, m[0].len, m[0].colour), (1, 1, SWATCHES[2]));
    }
    // The html did not change with the marks.
    let again = src.timeline(ConversationId(1));
    assert_eq!(rows, again, "the same rows from the cache");

    // Another revision with another pattern replaces them, an empty list removes them.
    take_queries(&sent);
    src.on_event(&UiEvent::FiltersChanged { revision: 2 });
    let asked = filter_queries(&sent);
    src.on_event(&result(asked[0], QueryData::Filters(marking_list("3", 3))));
    let counts = mark_counts(&src);
    assert_eq!(counts.iter().filter(|n| **n == 1).count(), 1, "{counts:?}");
    take_queries(&sent);
    src.on_event(&UiEvent::FiltersChanged { revision: 3 });
    let asked = filter_queries(&sent);
    src.on_event(&result(asked[0], QueryData::Filters(marking_list("", 4))));
    assert!(mark_counts(&src).iter().all(|n| *n == 0), "no filter: none");
}
