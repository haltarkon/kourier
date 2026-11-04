//! The WARN and ERROR lines of a test child, captured (TH4 round B: a test that expects "one WARN, no
//! content"). A tracing subscriber that keeps the MESSAGE of each event at WARN or above, nothing else:
//! the fields of an event are not read at all, so a value that reached a field would not be seen here
//! (the content rule is held by the source, `tests/builder.rs`, not by this capture). It is installed
//! as the GLOBAL default of the child process (the actor runs on its own thread), so it belongs in a test
//! that runs in its own `unshare` child, once, before the actor starts.

use std::sync::{Arc, Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// The captured lines: `(level, message)`.
#[derive(Clone, Default)]
pub struct Warns(Arc<Mutex<Vec<(Level, String)>>>);

impl Warns {
    /// Installs the capture as the process-wide subscriber; the handle reads what it caught.
    pub fn install() -> Self {
        let warns = Self::default();
        tracing::subscriber::set_global_default(Capture(warns.0.clone()))
            .expect("no other subscriber in the test child");
        warns
    }

    /// The messages of the lines at `level`, in order.
    pub fn at(&self, level: Level) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(l, _)| *l == level)
            .map(|(_, m)| m.clone())
            .collect()
    }
}

struct Capture(Arc<Mutex<Vec<(Level, String)>>>);

/// Reads the `message` field of an event and nothing else.
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= Level::WARN
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((*event.metadata().level(), message.0));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}
