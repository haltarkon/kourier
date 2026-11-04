//! The network's descriptor, the id of an own message, and its `spawn`, which starts the account
//! thread and no helper (TG-1 plan §5.4 T-b1, T-b2).

use std::time::Duration;

use kourier_backend_api::{Command, Exit, Network, StartMode};
use kourier_model::{
    ConvKey, EditTarget, HistorySource, MsgIdScope, NetworkKind, OutboxPolicy, UserId,
};
use kourier_telegram::{TelegramNetwork, TgParts};

use crate::support::{self, TEST_KEY};

fn network() -> TelegramNetwork {
    TelegramNetwork::new(TgParts::refusing(Default::default()))
}

#[test]
fn the_descriptor_is_experimental_qr_first_on_demand_and_warns() {
    let network = network();
    assert_eq!(network.kind(), NetworkKind::Telegram);
    let d = network.descriptor();
    assert_eq!(d.display_name, "Telegram");
    assert_eq!(d.icon_name, "telegram");
    // Registered only while `networks-on` names it, Absent from Add Account until TG-2 (D2).
    assert!(d.experimental, "Telegram must be experimental");
    // K-A's warning page asks first only for a descriptor with a notice.
    let notice = d.risk_notice.as_ref().expect("Telegram asks first");
    assert!(!notice.title.is_empty() && !notice.body.is_empty() && notice.url.is_none());
    let flows: Vec<_> = d.login_flows.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(flows, ["qr", "phone"], "the QR code comes first (TSC M2)");
    assert_eq!(d.history, HistorySource::OnDemand);
    assert!(!d.history_transfer, "the archive transfer is Signal's");
    assert_eq!(d.msg_id_scope, MsgIdScope::Conversation);
    assert_eq!(d.edit_target, EditTarget::Original);
    assert!(
        d.start_chat.is_empty(),
        "never a new chat (TS TG10 default)"
    );
    assert_eq!(d.outbox, OutboxPolicy::SIGNAL);
    assert_eq!(d.shutdown_grace, Some(Duration::from_secs(20)));
    assert!(
        d.receipts_always_sent,
        "Telegram's rules: read receipts, typing and online status are always sent (KT-1b)"
    );
    assert_eq!(format!("{network:?}"), "TelegramNetwork");
}

/// The descriptor twin (KT-1b, as Threema's L7): `kourier-fake` copies this descriptor for the fake
/// that plays Telegram (the fixture pages, the link tests, the run tool's rehearsal), words of the risk
/// notice included, with no import of this crate. Every field but four is equal; the id scope and the
/// edit target stay the Signal-like ones in the fake on purpose (the same script gives the same chats
/// and ids as the plain fake), and the fake states NO shutdown grace where this backend states 20 s
/// ("the fake stops at once": the core's limit follows the largest registered grace, so a fixture
/// page or window test that plays Telegram would allow a 20 s quit for nothing). The differences are
/// pinned here and then made equal to compare the rest in one assertion (a field the descriptor
/// gains later is compared too). Mutants: one word of the risk notice here; `receipts_always_sent`
/// false here; the grace changed on either side.
#[test]
fn the_fake_playing_telegram_copies_this_descriptor() {
    let real = network().descriptor().clone();
    let mut fake = kourier_fake::Flavor::SignalLike.playing_descriptor(NetworkKind::Telegram);
    assert_eq!(
        (fake.msg_id_scope, fake.edit_target, fake.outbox),
        (
            MsgIdScope::AccountAuthor,
            EditTarget::LatestRevision,
            OutboxPolicy::SIGNAL
        ),
        "the fake keeps the Signal-like numbers on purpose"
    );
    assert_eq!(fake.shutdown_grace, None, "the fake stops at once");
    assert_eq!(
        real.shutdown_grace,
        Some(Duration::from_secs(20)),
        "the real backend states its helper's close"
    );
    fake.msg_id_scope = real.msg_id_scope;
    fake.edit_target = real.edit_target;
    fake.outbox = real.outbox;
    fake.shutdown_grace = real.shutdown_grace;
    assert_eq!(fake, real);
    // (The words the warning page falls back to: both sides, by their literals.)
    let notice = real.risk_notice.as_ref().expect("a risk notice");
    assert_eq!(notice.title, "Before You Add Telegram");
    assert!(notice.body.starts_with("Kourier is not made by Telegram."));
    assert_eq!(notice.body.matches("\n\n").count(), 2, "three paragraphs");
}

#[test]
fn an_own_message_id_is_k_and_sixteen_hex_digits_and_never_a_tdlib_number() {
    let network = network();
    let conv = ConvKey::Direct(UserId::parse("tg:77").expect("a user id"));
    let now_ms = 1_792_000_000_123;
    let a = network.allocate_message_id(&[], &conv, now_ms);
    let b = network.allocate_message_id(&[], &conv, now_ms);
    for id in [&a, &b] {
        assert_eq!(id.local.len(), 17, "not k and 16 digits");
        assert!(
            id.local.starts_with('k'),
            "an own id must not look like TDLib's"
        );
        assert!(
            id.local[1..]
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "not lower-case hex"
        );
        assert_eq!(id.sent_at, 1_792_000_000_123);
    }
    assert_ne!(a.local, b.local, "two ids of one instant are equal");
}

#[test]
fn spawn_starts_no_helper_and_a_shutdown_ends_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = support::rt();
    rt.block_on(async {
        let mut run = support::start(dir.path(), StartMode::Fresh, TEST_KEY);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(run.helpers_made(), 0, "spawn started a helper");
        run.ends
            .commands
            .send(Command::Shutdown)
            .await
            .expect("the actor takes commands");
        let exit = tokio::time::timeout(Duration::from_secs(1), &mut run.handle.exited)
            .await
            .expect("the actor did not end within 1 s of a shutdown");
        assert_eq!(Exit::from_channel(exit), Exit::Shutdown);
        assert_eq!(run.helpers_made(), 0, "the actor started a helper");
        if let Some(thread) = run.handle.thread.take() {
            thread.join().expect("the account thread ended cleanly");
        }
    });
}
