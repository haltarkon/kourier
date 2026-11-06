//! The log lines of a test child, captured (TH4 round B: a test that expects "one WARN, no content"; TH6 C1: the run
//! tool's INFO lines and their fields). A tracing subscriber that keeps each event at its level or above: its level, its
//! target, its MESSAGE and its fields written as the app's plain log writes them (`name=value`, a value as tracing's
//! format writes it: a number bare, a `%` word bare, a `&str` field in quotes). [`Warns::install`] keeps WARN and
//! above (the content rule of a WARN is held by the source, `tests/builder.rs`); [`Warns::install_info`] keeps INFO and
//! above, for a test that reads the count and kind lines and searches every line of the crate for an ID. It is installed
//! as the GLOBAL default of the child process (the actor runs on its own thread), so it belongs in a test that runs in
//! its own `unshare` child, once, before the actor starts.

use std::sync::{Arc, Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// One captured line.
#[derive(Clone, Debug)]
pub struct Line {
    pub level: Level,
    /// The event's target (the module path of the call).
    pub target: String,
    pub message: String,
    /// The fields but the message, in the call's order.
    pub fields: Vec<(String, String)>,
}

impl Line {
    /// The value of field `name`, as the log writes it.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The line as the app's plain log writes it after the level and target: the message, then `name=value` each.
    pub fn text(&self) -> String {
        let mut text = self.message.clone();
        for (name, value) in &self.fields {
            text.push_str(&format!(" {name}={value}"));
        }
        text
    }
}

/// The captured lines.
#[derive(Clone, Default)]
pub struct Warns(Arc<Mutex<Vec<Line>>>);

impl Warns {
    /// Installs the capture of WARN and above as the process-wide subscriber; the handle reads what it caught.
    pub fn install() -> Self {
        Self::install_from(Level::WARN, None)
    }

    /// TH6 C1: the same with INFO and above.
    pub fn install_info() -> Self {
        Self::install_from(Level::INFO, None)
    }

    /// TH6 L7: INFO and above, and DEBUG too for the events whose target starts with `debug_of` (the leader's
    /// discards say their rule at DEBUG).
    pub fn install_info_and_debug_of(debug_of: &'static str) -> Self {
        Self::install_from(Level::INFO, Some(debug_of))
    }

    fn install_from(least: Level, debug_of: Option<&'static str>) -> Self {
        let warns = Self::default();
        tracing::subscriber::set_global_default(Capture {
            lines: warns.0.clone(),
            least,
            debug_of,
        })
        .expect("no other subscriber in the test child");
        warns
    }

    /// The messages of the lines at `level`, in order.
    pub fn at(&self, level: Level) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.level == level)
            .map(|l| l.message)
            .collect()
    }

    /// Every captured line, in order.
    pub fn lines(&self) -> Vec<Line> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct Capture {
    lines: Arc<Mutex<Vec<Line>>>,
    least: Level,
    /// The target prefix whose DEBUG events are kept too.
    debug_of: Option<&'static str>,
}

/// Reads the message and every other field of an event.
#[derive(Default)]
struct Fields {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push((field.name().to_owned(), format!("{value:?}")));
        }
    }
}

impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= self.least
            || (*metadata.level() <= Level::DEBUG
                && self
                    .debug_of
                    .is_some_and(|prefix| metadata.target().starts_with(prefix)))
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Line {
                level: *event.metadata().level(),
                target: event.metadata().target().to_owned(),
                message: fields.message,
                fields: fields.fields,
            });
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}
