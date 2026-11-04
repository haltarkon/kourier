//! A tracing subscriber for the tests that read what a piece of code logs: every event of the thread
//! that installed it, as `message=".." name=value ..` text (the message first). `tracing::subscriber::
//! set_default` holds it for the guard's life, on the thread that set it (a `#[tokio::test]` runs on
//! one thread, so an async test sees the events of its own futures).

use std::fmt::Debug;
use std::sync::{Arc, Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber, span};

/// The events seen so far, one text per event.
#[derive(Clone, Default)]
pub(crate) struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    /// A copy of what was logged.
    pub(crate) fn all(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The events whose text holds `needle`.
    pub(crate) fn containing(&self, needle: &str) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|l| l.contains(needle))
            .collect()
    }
}

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
        struct All(Vec<String>);
        impl Visit for All {
            fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                self.0.push(format!("{}={value:?}", field.name()));
            }
        }
        let mut all = All(Vec::new());
        event.record(&mut all);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(all.0.join(" "));
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}
