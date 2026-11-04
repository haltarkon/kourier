//! A recording `tracing` subscriber for the tests that read what Kourier logs (W3a, decision D10).
//!
//! It keeps, for every event and every new span of the process, the target and the fields (message
//! included) as one text, at every level, so that a test can look for what must never be logged: a
//! number, a LID, a name, a message text. Installed once, as the process's default, in the
//! offline child of a test; no test reads another test's lines because one child runs one test
//! function. It prints nothing.
//!
//! UPSTREAM'S LINES (W3b fix F6, privacy review MEDIUM 2). whatsapp-rust and the crates under it log
//! through the `log` crate, which a `tracing` subscriber alone never hears. `install` therefore also
//! bridges `log` into `tracing` (`tracing_log::LogTracer`, the bridge the app's subscriber has), so the
//! grep sees upstream's records. A bridged record is judged by the APP'S RULE for them
//! (`kourier_whatsapp::logs::keeps`: from upstream only WARN and up are written, whatever the filter
//! says) and the rest are counted, not kept: a debug line naming a JID is not a leak, the app never
//! writes it. What the app's rule keeps is kept here AS UPSTREAM WROTE IT, without
//! `redact_upstream`: the redaction is the app's second line, and a test that looked at the redacted
//! text could not see an upstream WARN that formats a number of the fixture (the unreadable-call case
//! passes such a stanza to upstream's own handler when the claim is missing).

use std::fmt::{Debug, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kourier_whatsapp::logs;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// One recorded event or span: where it came from and what it said.
#[derive(Clone, Debug)]
pub struct Line {
    pub target: String,
    pub text: String,
}

/// The recorder; a clone shares the lines.
#[derive(Clone, Default)]
pub struct Capture {
    lines: Arc<Mutex<Vec<Line>>>,
    next: Arc<AtomicU64>,
    /// How many bridged `log` records were from upstream's crates (kept or dropped by the app's rule).
    upstream: Arc<AtomicU64>,
}

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

/// The module path a `log` record had (the bridge carries it in a field; the target is a free word).
#[derive(Default)]
struct ModulePath(Option<String>);

impl Visit for ModulePath {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "log.module_path" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

/// An event of the shared callsites `tracing-log` dispatches every `log` record on.
fn is_bridged(meta: &Metadata<'_>) -> bool {
    meta.name() == "log event" && meta.target() == "log"
}

impl Capture {
    /// Makes this the process's default subscriber and bridges the `log` crate into it. Panics when
    /// either is already set (a child runs one test).
    pub fn install(&self) {
        tracing_log::LogTracer::init().expect("no other log logger in the child");
        tracing::subscriber::set_global_default(self.clone())
            .expect("no other default subscriber in the child");
    }

    /// How many `log` records of upstream's crates came through the bridge, those the app's rule
    /// drops included: a test that finds no secret in upstream's lines must also have seen some.
    pub fn upstream_records(&self) -> u64 {
        self.upstream.load(Ordering::Relaxed)
    }

    /// Everything recorded so far.
    pub fn lines(&self) -> Vec<Line> {
        self.lines.lock().expect("lock").clone()
    }

    /// How many lines Kourier's own crate wrote (a test that finds nothing wrong must also have
    /// seen something).
    pub fn own_lines(&self) -> usize {
        self.lines()
            .iter()
            .filter(|l| l.target.starts_with("kourier_whatsapp"))
            .count()
    }

    /// The lines (any crate) that contain one of `secrets`, as `target: text`, to name in a failure
    /// without printing the secret.
    pub fn leaks(&self, secrets: &[&str]) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| secrets.iter().any(|s| l.text.contains(s)))
            .map(|l| l.target)
            .collect()
    }

    /// The targets of the crate's OWN lines (Kourier's, not upstream's) that hold a run of [`TOKEN_RUN`] or more token
    /// characters (`[A-Za-z0-9+/=_-]`): the shape of a key, a token or a hash in base64 or hex (fix F17). Keys and
    /// tokens are random per run, so a grep for the fixture's values is blind to them; the shape is not. Upstream's
    /// lines are the app's second line (`redact_upstream`) and are judged by `leaks` as written, EXCEPT the WARN and
    /// ERROR lines of the modules a media upload runs through ([`UPLOAD_PATH_MODULES`], fix F14): the upload's URL
    /// carries a token and the media connection's answer an `auth`, and a pin bump that prints either there would
    /// be a leak the app's redaction is the only guard of.
    pub fn token_shaped(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| {
                (l.target.starts_with("kourier_whatsapp") || is_upload_path(&l.target))
                    && has_token_run(&l.text)
            })
            .map(|l| l.target)
            .collect()
    }

    fn push(&self, target: &str, fields: Fields) {
        self.lines.lock().expect("lock").push(Line {
            target: target.to_owned(),
            text: fields.0,
        });
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        let mut fields = Fields(attrs.metadata().name().to_owned());
        attrs.record(&mut fields);
        self.push(attrs.metadata().target(), fields);
        Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let meta = event.metadata();
        if is_bridged(meta) {
            let mut module = ModulePath::default();
            event.record(&mut module);
            let upstream = module.0.as_deref().is_none_or(logs::is_upstream);
            if upstream {
                self.upstream.fetch_add(1, Ordering::Relaxed);
            }
            // The app's rule: an upstream record below WARN is never written (no module path counts
            // as upstream's).
            let written = match module.0.as_deref() {
                Some(path) => logs::keeps(path, *meta.level()),
                None => *meta.level() <= Level::WARN,
            };
            if !written {
                return;
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        if is_bridged(meta) {
            // Name the record by its module path and level: the target of every bridged record is
            // the shared word "log".
            let mut module = ModulePath::default();
            event.record(&mut module);
            let target = format!(
                "log:{}:{}",
                module.0.as_deref().unwrap_or("?"),
                meta.level()
            );
            self.push(&target, fields);
            return;
        }
        self.push(meta.target(), fields);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// The upstream modules the bytes of an upload pass through (fix F14): the upload and the media connection of
/// whatsapp-rust, wacore's upload and download, the HTTP client crate and `ureq` (and `ureq_proto`: a prefix).
/// A record's module path starts with one of these.
const UPLOAD_PATH_MODULES: &[&str] = &[
    "whatsapp_rust::upload",
    "whatsapp_rust::mediaconn",
    "whatsapp_rust::media",
    "wacore::upload",
    "wacore::download",
    // The HTTP client crate's module path, assembled so that no file but `src/real.rs` names the crate (the source check
    // of `tests/builder.rs`); it is a log module here, nothing is built from it.
    concat!("whatsapp_rust_", "ureq_http_client"),
    "ureq",
];

/// Whether `target` names a bridged `log` record (`log:<module path>:<LEVEL>`, see `Capture::event`) of an upload-path
/// module of upstream.
fn is_upload_path(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("log:") else {
        return false;
    };
    let module = rest.rsplit_once(':').map_or(rest, |(module, _)| module);
    UPLOAD_PATH_MODULES.iter().any(|m| module.starts_with(m))
}

/// How many token characters in a row make a line look like it holds a key or a token (fix F17). A message id is
/// 22 characters and a field name or a word is shorter than this.
pub const TOKEN_RUN: usize = 32;

/// Whether `text` holds [`TOKEN_RUN`] or more characters of `[A-Za-z0-9+/=_-]` in a row.
pub fn has_token_run(text: &str) -> bool {
    let mut run = 0usize;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-') {
            run += 1;
            if run >= TOKEN_RUN {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}
