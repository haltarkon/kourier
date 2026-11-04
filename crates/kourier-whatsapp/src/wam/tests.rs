//! The statistics watch on hand-made frames (W2, chunk C4c; T-9, second half). The frames are built
//! with upstream's own stanza builder and marshalled the way its send path writes them, so the
//! decoder is shown on the real wire form. That the real client's frames reach the watch is shown by
//! `session::client::tests` (the handler) and, with a server, by the scripted fixture (chunk C5).

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber, span};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::marshal::marshal;

use super::*;

/// The namespace is spelled by this file's subject only; a test builds it the same way the source
/// check does, so no other file of `src/` has to name it.
fn stats_namespace() -> String {
    STATS_NAMESPACE.to_owned()
}

fn frame(tag: &'static str, namespace: Option<&str>) -> Vec<u8> {
    let mut node = NodeBuilder::new(tag).attr("type", "set").attr("id", "7");
    if let Some(namespace) = namespace {
        node = node.attr("xmlns", namespace);
    }
    marshal(&node.build()).expect("marshals")
}

#[test]
fn a_statistics_stanza_is_found_by_its_namespace_and_nothing_else_is() {
    assert_eq!(
        look_at(&frame("iq", Some(&stats_namespace()))),
        Look::Statistics
    );
    // Any root that carries the namespace counts, not only an `iq`.
    assert_eq!(
        look_at(&frame("message", Some(&stats_namespace()))),
        Look::Statistics
    );
    // The same shape in another namespace, and stanzas with none.
    assert_eq!(look_at(&frame("iq", Some("usync"))), Look::Other);
    assert_eq!(look_at(&frame("iq", Some("w:p"))), Look::Other);
    assert_eq!(look_at(&frame("message", None)), Look::Other);
    assert_eq!(look_at(&frame("ack", None)), Look::Other);
    // A namespace that only starts like it is not it.
    assert_eq!(
        look_at(&frame("iq", Some(&format!("{}2", stats_namespace())))),
        Look::Other
    );
}

#[test]
fn only_the_root_counts_and_a_big_stanza_is_not_read_whole() {
    // A statistics node below an ordinary root is not WAM's shape; the head is all that is read.
    let nested = NodeBuilder::new("iq")
        .attr("xmlns", "usync")
        .children([NodeBuilder::new("query")
            .attr("xmlns", stats_namespace())
            .build()])
        .build();
    assert_eq!(look_at(&marshal(&nested).expect("marshals")), Look::Other);
    // A root that carries the namespace is found however much hangs below it.
    let children: Vec<_> = (0..2_000)
        .map(|i| {
            NodeBuilder::new("item")
                .attr("n", i.to_string())
                .bytes(vec![0xAB; 64])
                .build()
        })
        .collect();
    let big = NodeBuilder::new("iq")
        .attr("xmlns", stats_namespace())
        .children(children)
        .build();
    let bytes = marshal(&big).expect("marshals");
    assert!(bytes.len() > 100_000, "the frame is big: {}", bytes.len());
    assert_eq!(look_at(&bytes), Look::Statistics);
    // ...and a truncated copy still shows its head.
    assert_eq!(look_at(&bytes[..64]), Look::Statistics);
}

#[test]
fn bytes_that_are_not_a_stanza_are_unreadable_and_never_statistics() {
    assert_eq!(look_at(&[]), Look::Unreadable);
    assert_eq!(look_at(&[0]), Look::Unreadable);
    assert_eq!(look_at(&[0xFF; 8]), Look::Unreadable);
    // Node bytes without the format byte are not what the send path hands over.
    let bytes = frame("iq", Some(&stats_namespace()));
    assert_eq!(look_at(&bytes[1..]), Look::Unreadable);
}

/// Collects the events of the thread that installed it.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<(Level, String)>>>);

impl Subscriber for Lines {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        struct Message(String);
        impl Visit for Message {
            fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .expect("lock")
            .push((*event.metadata().level(), message.0));
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

#[test]
fn a_statistics_frame_is_counted_and_logged_as_an_error_and_other_frames_only_counted_as_seen() {
    let lines = Lines::default();
    let watch = Watch::default();
    let before = wam_sent();
    tracing::subscriber::with_default(lines.clone(), || {
        watch.look(&frame("iq", Some("usync")));
        watch.look(&frame("message", None));
        assert_eq!((watch.seen(), watch.sent()), (2, 0));
        assert!(
            lines.0.lock().expect("lock").is_empty(),
            "a quiet frame logs nothing"
        );

        watch.look(&frame("iq", Some(&stats_namespace())));
    });
    assert_eq!((watch.seen(), watch.sent(), watch.unreadable()), (3, 1, 0));
    assert!(wam_sent() > before, "the process-wide count moved too");
    let lines = lines.0.lock().expect("lock");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, Level::ERROR);
    assert!(lines[0].1.starts_with(LOG_MARK), "{lines:?}");
    // The line says that it happened, not what was sent.
    assert!(!lines[0].1.contains(&stats_namespace()), "{lines:?}");
}

#[test]
fn unreadable_frames_are_counted_and_said_once() {
    let lines = Lines::default();
    let watch = Watch::default();
    let before = wam_sent();
    tracing::subscriber::with_default(lines.clone(), || {
        for _ in 0..3 {
            watch.look(&[0xFF, 0x01]);
        }
    });
    assert_eq!((watch.seen(), watch.sent(), watch.unreadable()), (3, 0, 3));
    assert!(wam_sent() >= before);
    let lines = lines.0.lock().expect("lock");
    assert_eq!(lines.len(), 1, "once, not per frame: {lines:?}");
    assert_eq!(lines[0].0, Level::WARN);
}
