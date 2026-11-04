//! A tracing subscriber for the tests that read what this crate logs (decisions D3 M5): every event
//! and every new span, as `name=value ..` text, at every level. `tracing::subscriber::set_default`
//! holds it on the thread that set it; the link's threads take the subscriber of the thread that
//! started the link, so a test sees them too.

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

    fn push(&self, line: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }
}

struct All(Vec<String>);

impl Visit for All {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

impl Subscriber for Lines {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &span::Attributes<'_>) -> span::Id {
        let mut all = All(vec![format!("span={}", attrs.metadata().name())]);
        attrs.record(&mut all);
        self.push(all.0.join(" "));
        span::Id::from_u64(1)
    }
    fn record(&self, _: &span::Id, values: &span::Record<'_>) {
        let mut all = All(vec!["span-record".to_owned()]);
        values.record(&mut all);
        self.push(all.0.join(" "));
    }
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut all = All(vec![format!("target={}", event.metadata().target())]);
        event.record(&mut all);
        self.push(all.0.join(" "));
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}
