//! The log lines of whatsapp-rust and the crates under it (W2b, B1; architecture §9.1) and of libthreema
//! (TH4 chunk L, decision C11).
//!
//! Those crates log through `log` (whatsapp-rust: free words as targets, "Client/AppState") or `tracing`
//! (libthreema: INFO spans whose fields print protocol objects, `trace!(?result)` events, `warn!`/`error!`
//! with their data in fields), and some of their lines name people: a number, a LID, the pair code, a
//! stanza with a push name. `kourier_whatsapp::logs` and `kourier_threema::logs` hold the rules (which
//! crates are upstream's, the level rule, for WhatsApp the redaction of a MESSAGE); each names the
//! transport crates under its own network (WhatsApp's `rustls`, Threema's `rustls`, `tungstenite`,
//! `reqwest`, `hyper`, `h2` ...), so neither rule depends on the other network being built in. This file puts
//! them into the two `fmt` layers of [`super::init`], each network's arm under its own cargo feature:
//!
//! - [`Rule`] is a per-layer filter in FRONT of the layer's `EnvFilter`. It judges each record, and each
//!   SPAN, by its MODULE PATH (the target is a free word, the formatted line carries neither), never on a
//!   formatted line: an upstream record below WARN is dropped whatever `KOURIER_LOG` says, so the user's
//!   filter can narrow what upstream says but never widen it (`BackendConfig::log_filter` has no reader
//!   at all). A record WITHOUT a module path counts as upstream's: the safe side.
//! - [`format_upstream`] writes an upstream record that passed as time, level, target and its message
//!   and nothing else (a native `tracing` event of those crates may carry fields, and the `log` bridge
//!   carries `log.*` fields; no span is written either). WhatsApp's message goes through
//!   `redact_upstream`; libthreema's and the dial's transport crates' are written as they are (libthreema's
//!   call sites keep data out of the message, and the transport crates' WARN and ERROR lines say what a
//!   socket or a frame did).
//!   Every other record is written as the default format writes it. The writer's own `redact` still
//!   runs on every finished line, upstream's included.
//!
//! The five callsites of `tracing-log` (one per level, target "log") are shared by every `log` record in
//! the process: libsignal's too. The filter answers `Interest::sometimes` for them and decides on the
//! record itself, because a `never` there would silence libsignal's records of that level. The bridge asks
//! twice before it dispatches a record: first with the `log::Metadata` (target and level, NO module path),
//! then with the whole record; only the second question can be answered, so the first is a yes.

use std::fmt;

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime as _, SystemTime};
use tracing_subscriber::layer::{Context, Filter};

/// WhatsApp's arm of the rule: `kourier_whatsapp::logs` in a build that has the backend, else a rule
/// that owns no module and holds nothing back.
#[cfg(feature = "whatsapp")]
use kourier_whatsapp::logs as whatsapp;
#[cfg(not(feature = "whatsapp"))]
mod whatsapp {
    use tracing::Level;

    pub(super) fn is_upstream(_: &str) -> bool {
        false
    }

    pub(super) fn keeps(_: &str, _: Level) -> bool {
        true
    }

    pub(super) fn redact_upstream(message: &str) -> String {
        message.to_owned()
    }
}

/// Threema's arm of the rule: `kourier_threema::logs` (libthreema) in a build that has the backend, else
/// a rule that owns no module and holds nothing back.
#[cfg(feature = "threema")]
use kourier_threema::logs as threema;
#[cfg(not(feature = "threema"))]
mod threema {
    use tracing::Level;

    pub(super) fn is_upstream(_: &str) -> bool {
        false
    }

    pub(super) fn keeps(_: &str, _: Level) -> bool {
        true
    }
}

/// Whether a record of `module_path` and `level` is written at all: every network's rule must keep it.
/// No module path: upstream's rule.
pub(super) fn written(module_path: Option<&str>, level: Level) -> bool {
    match module_path {
        Some(module) => whatsapp::keeps(module, level) && threema::keeps(module, level),
        None => level <= Level::WARN,
    }
}

/// Whether the record is upstream's (of any network built in), so it is written as its message alone.
/// No module path: it is.
pub(super) fn is_upstream(module_path: Option<&str>) -> bool {
    module_path.is_none_or(|module| whatsapp::is_upstream(module) || threema::is_upstream(module))
}

/// The message of an upstream record as it is written. WhatsApp's (and a record without a module path)
/// goes through `redact_upstream`: so does a module both rules own (`rustls` in a build with both
/// backends). libthreema's and the dial's other transport crates' are written as they are: libthreema's
/// `warn!` and `error!` call sites carry their data in fields, which are never written, and a WhatsApp
/// rule would only mangle a sentence.
fn written_message(module_path: Option<&str>, message: &str) -> String {
    if module_path.is_none_or(whatsapp::is_upstream) {
        whatsapp::redact_upstream(message)
    } else {
        message.to_owned()
    }
}

/// Metadata made for a `log` record (`tracing-log`: "log record" for the question before the record
/// is dispatched, "log event" for the shared callsites of the event itself).
fn is_bridge_metadata(meta: &Metadata<'_>) -> bool {
    matches!(meta.name(), "log record" | "log event")
}

/// An event of the shared `log` callsites: its real module path is in a field.
fn is_bridged(event: &Event<'_>) -> bool {
    let meta = event.metadata();
    meta.name() == "log event" && meta.target() == "log"
}

/// What an event says of itself: its message and, from a `log` record, the module path and target
/// the record had.
#[derive(Default)]
struct Seen {
    message: Option<String>,
    module_path: Option<String>,
    target: Option<String>,
}

impl Visit for Seen {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = Some(value.to_owned()),
            "log.module_path" => self.module_path = Some(value.to_owned()),
            "log.target" => self.target = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

fn seen(event: &Event<'_>) -> Seen {
    let mut seen = Seen::default();
    event.record(&mut seen);
    seen
}

/// The per-layer filter that holds upstream's records to the rule (see the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Rule;

impl<S: Subscriber> Filter<S> for Rule {
    /// The rule only ever narrows what the `EnvFilter` behind it lets through, so it asks for the
    /// most (`TRACE`) and the combined filter's hint is the `EnvFilter`'s. Without this the hint is
    /// `None`, `And` takes `None` for the whole, and the `log` crate's maximum level goes to TRACE.
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }

    fn enabled(&self, meta: &Metadata<'_>, _: &Context<'_, S>) -> bool {
        if meta.module_path().is_none() && is_bridge_metadata(meta) {
            // The question before the record is dispatched has no module path: the event decides.
            return true;
        }
        written(meta.module_path(), *meta.level())
    }

    fn callsite_enabled(&self, meta: &'static Metadata<'static>) -> Interest {
        if is_bridge_metadata(meta) {
            return Interest::sometimes();
        }
        // A callsite of `tracing`'s own macros has its module path for good.
        if written(meta.module_path(), *meta.level()) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn event_enabled(&self, event: &Event<'_>, _: &Context<'_, S>) -> bool {
        let level = *event.metadata().level();
        if is_bridged(event) {
            written(seen(event).module_path.as_deref(), level)
        } else {
            written(event.metadata().module_path(), level)
        }
    }
}

/// Writes `event` as time, level, target and its message ([`written_message`]), and nothing else,
/// when it is upstream's (a native `tracing` event of those crates may carry fields, and the `log`
/// bridge carries `log.*` fields: none of them is written, and neither is a span's). `None` when the
/// event is not upstream's: the caller writes it as every other record.
pub(super) fn format_upstream(writer: &mut Writer<'_>, event: &Event<'_>) -> Option<fmt::Result> {
    let seen = if is_bridged(event) {
        let seen = seen(event);
        is_upstream(seen.module_path.as_deref()).then_some(seen)?
    } else {
        is_upstream(event.metadata().module_path()).then(|| seen(event))?
    };
    let module_path = if is_bridged(event) {
        seen.module_path.as_deref()
    } else {
        event.metadata().module_path()
    };
    let target = match (is_bridged(event), seen.target.as_deref()) {
        (true, Some(target)) => target,
        (true, None) => "log",
        (false, _) => event.metadata().target(),
    };
    Some(
        SystemTime
            .format_time(writer)
            .and_then(|()| {
                write!(
                    writer,
                    " {:>5} {target}: {}",
                    event.metadata().level(),
                    written_message(module_path, seen.message.as_deref().unwrap_or(""))
                )
            })
            .and_then(|()| writeln!(writer)),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::{Layer, Registry};

    use super::*;

    /// The level rule for every kind of record: libsignal's and Kourier's own as the filters say,
    /// upstream's (whatsapp-rust's and libthreema's, each under its own feature) from WARN up, a record
    /// without a module path as upstream's.
    #[test]
    fn upstream_and_unplaced_records_are_cut_to_warn() {
        for level in [Level::TRACE, Level::DEBUG, Level::INFO] {
            assert!(written(Some("libsignal_net::chat"), level));
            assert!(written(Some("kourier_core::ingest"), level));
            assert!(!written(None, level));
            // A longer crate name is not libthreema's: written as the filters say.
            assert!(written(Some("libthreemax::y"), level));
            #[cfg(feature = "whatsapp")]
            for module in [
                "whatsapp_rust::client::lid_pn",
                "wacore::net::noise",
                "whatsapp_rust_tokio_transport",
            ] {
                assert!(!written(Some(module), level), "{module} at {level}");
            }
            #[cfg(feature = "threema")]
            for module in [
                "libthreema",
                "libthreema::d2m",
                "libthreema::d2d_rendezvous",
                "libthreema_macros::y",
            ] {
                assert!(!written(Some(module), level), "{module} at {level}");
            }
            // The dial's transport crates are Threema's rule too, whether or not WhatsApp is built in
            // (`rustls` is in both lists).
            #[cfg(feature = "threema")]
            for module in [
                "rustls::client::hs",
                "tokio_rustls::client",
                "tokio_tungstenite",
                "tungstenite::handshake::client",
                "reqwest::connect",
                "hyper::proto::h1::conn",
                "hyper_util::client::legacy::pool",
                "h2::codec::framed_read",
            ] {
                assert!(!written(Some(module), level), "{module} at {level}");
            }
            // ... and a longer crate name is not theirs.
            assert!(written(Some("reqwest_middleware::y"), level));
            assert!(written(Some("hyperx::y"), level));
        }
        for level in [Level::WARN, Level::ERROR] {
            assert!(written(None, level));
            assert!(written(Some("libthreemax::y"), level));
            #[cfg(feature = "whatsapp")]
            assert!(written(Some("whatsapp_rust::client::lid_pn"), level));
            #[cfg(feature = "threema")]
            for module in [
                "libthreema::d2m",
                "libthreema_macros::y",
                "reqwest::connect",
                "tungstenite::protocol",
                "h2::proto::connection",
            ] {
                assert!(written(Some(module), level), "{module} at {level}");
            }
        }
        assert!(is_upstream(None));
        #[cfg(feature = "whatsapp")]
        assert!(is_upstream(Some("wacore::net::noise")));
        #[cfg(feature = "threema")]
        {
            assert!(is_upstream(Some("libthreema::d2m")));
            assert!(is_upstream(Some("libthreema_macros::y")));
            for module in [
                "rustls::client::hs",
                "tokio_tungstenite",
                "reqwest::connect",
                "h2::codec",
            ] {
                assert!(is_upstream(Some(module)), "{module}");
            }
        }
        assert!(!is_upstream(Some("libsignal_net::chat")));
        // Whole crate names only.
        assert!(!is_upstream(Some("whatsapp_rustic::x")));
        assert!(!is_upstream(Some("libthreemax::y")));
    }

    /// Only WhatsApp's messages (and those of a record without a module path) go through
    /// `redact_upstream`; libthreema's is written as it is.
    #[test]
    fn only_whatsapps_messages_are_redacted() {
        let text = "frame \"quoted\" 4096000 for phone: 4915123456789";
        #[cfg(feature = "threema")]
        for module in [
            "libthreema::d2m",
            "libthreema_macros::y",
            // The dial's transport crates that WhatsApp does not own: as they are, in every build.
            "tokio_tungstenite",
            "reqwest::connect",
            "hyper::proto::h1::conn",
            "h2::codec",
        ] {
            assert_eq!(written_message(Some(module), text), text, "{module}");
        }
        #[cfg(feature = "whatsapp")]
        for module in [Some("whatsapp_rust::client"), None] {
            let cut = written_message(module, text);
            assert_ne!(cut, text, "{module:?}");
            assert!(!cut.contains("4915123456789"), "{module:?}: {cut}");
        }
    }

    /// Counts the events a layer is given.
    struct Count(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for Count {
        fn on_event(&self, _: &Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The five shared callsites of the `log` bridge (target "log", the name "log event") are never
    /// decided when a callsite registers, because the module path of the record is not in them: a
    /// callsite of that shape that registered as `never` would silence every record of its level,
    /// libsignal's too. (`tracing-log` 0.2 never registers them, so the bridge itself cannot show it;
    /// the rule is for a version that does.) A callsite made by `tracing`'s macros, by contrast, has
    /// its module path for good and is decided at once.
    #[test]
    fn the_bridges_shared_callsites_are_not_decided_when_they_register() {
        let seen = Arc::new(AtomicUsize::new(0));
        let subscriber = Registry::default().with(Count(seen.clone()).with_filter(Rule));
        tracing::subscriber::with_default(subscriber, || {
            tracing::event!(
                name: "log event",
                target: "log",
                Level::INFO,
                log.module_path = "libsignal_net::chat",
                "a record of libsignal"
            );
        });
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }
}
