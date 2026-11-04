//! `LogFacts` says the kind of a ban and the cause of a logout and never the network's own words.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber, span};

use super::*;
use crate::states::state_of;
use crate::translate::{Banned, Logout, LogoutShape, WaBan, WaSignal};

/// Collects every field of every event of the thread that installed it, as `name=value` text.
#[derive(Clone, Default)]
struct Fields(Arc<Mutex<Vec<String>>>);

impl Subscriber for Fields {
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
        self.0.lock().expect("lock").push(all.0.join(" "));
    }
    fn enter(&self, _: &span::Id) {}
    fn exit(&self, _: &span::Id) {}
}

#[test]
fn the_state_is_logged_with_its_kind_and_cause_and_without_the_networks_words() {
    let ban = state_of(&WaSignal::Banned(Banned {
        ban: WaBan::SameMessageToMany,
        until_ms: 1_700_086_400_000,
        explanation: Some("Secret sentence of the ban.".to_owned()),
        appeal_url: Some("https://faq.example.invalid/secret-page".to_owned()),
    }))
    .expect("a state");
    let out = state_of(&WaSignal::LoggedOut(Logout {
        shape: LogoutShape::RemovedByPhone,
        violation: None,
        explanation: Some("Secret sentence of the logout.".to_owned()),
    }))
    .expect("a state");

    let lines = Fields::default();
    tracing::subscriber::with_default(lines.clone(), || {
        let mut facts = LogFacts;
        facts.state(&ban);
        facts.state(&out);
        facts.hold(Some(AccountHold {
            until_ms: None,
            reason: kourier_model::HoldReason::ReachOutLocked,
        }));
    });
    let lines = lines.0.lock().expect("lock").clone();
    assert_eq!(lines.len(), 3, "{lines:?}");
    // The kind and the cause are said ...
    assert!(lines[0].contains("Banned(SameMessageToMany)"), "{lines:?}");
    assert!(lines[1].contains("RemovedOnPhone"), "{lines:?}");
    // ... and not one word the network chose, nor its page.
    for line in &lines {
        for word in ["Secret", "sentence", "secret-page", "faq.example"] {
            assert!(!line.contains(word), "{word} in {line}");
        }
    }
}
