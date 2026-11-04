//! More accounts for the pages of blended chats (`FixtureOptions::accounts`, `--page=accounts`,
//! `--page=accountsdrawer`): a second account of the test network with five chats of its own
//! (one pinned, three messages unread) and a REGISTERED fake that plays WhatsApp with three
//! (one pinned, one message unread). Their chats are dated between those of the fixture account
//! (older than its newest message), so the blended list interleaves the three. Every name and id
//! is synthetic. Used by those two pages and the tests of the choice only: no other page sets
//! the option, so no other picture changes.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{CoreHandle, UiCommand};
use kourier_fake::scripts::{own, remote, signal_features};
use kourier_fake::{FakeNetwork, Script};
use kourier_model::{
    Body, ContactPatch, ConvKey, ConversationPatch, Member, MemberList, MemberRole, NetworkKind,
    Patch, UserId,
};

use crate::app::AppError;

/// The accounts the options add: the second account's script (it is pushed to the plain fake
/// before its `AddAccount`) and the fake that plays WhatsApp (registered at the start).
#[derive(Clone)]
pub(crate) struct Extra {
    /// The script of the second account of the test network.
    pub second: Script,
    /// The registered fake playing WhatsApp, with the script of its one account.
    pub whatsapp: Option<FakeNetwork>,
}

/// What a chat of an extra account holds.
struct Chat {
    /// The people: one for a direct chat, three for the group.
    people: &'static [(&'static str, u64)],
    /// The group's title, `None` for a direct chat.
    group: Option<&'static str>,
    pinned: bool,
    /// The newest messages, from the other side, that are unread.
    unread: usize,
    /// The place among the nine extra chats (0 = the newest), which sets its date.
    slot: usize,
}

const SECOND: &[Chat] = &[
    Chat {
        people: &[("Marta Voss", 1)],
        group: None,
        pinned: true,
        unread: 0,
        slot: 1,
    },
    Chat {
        people: &[("Tobin Keel", 2)],
        group: None,
        pinned: false,
        unread: 2,
        slot: 0,
    },
    Chat {
        people: &[("Ilse Brandt", 3), ("Corin Hale", 4), ("Pavel Dorn", 5)],
        group: Some("Release Crew"),
        pinned: false,
        unread: 1,
        slot: 3,
    },
    Chat {
        people: &[("Edda Lund", 6)],
        group: None,
        pinned: false,
        unread: 0,
        slot: 5,
    },
    Chat {
        people: &[("Soren Pike", 7)],
        group: None,
        pinned: false,
        unread: 0,
        slot: 7,
    },
];

const WHATSAPP: &[Chat] = &[
    Chat {
        people: &[("Nadia Ferro", 1)],
        group: None,
        pinned: true,
        unread: 0,
        slot: 2,
    },
    Chat {
        people: &[("Rune Alder", 2)],
        group: None,
        pinned: false,
        unread: 1,
        slot: 4,
    },
    Chat {
        people: &[("Wilma Quade", 3)],
        group: None,
        pinned: false,
        unread: 0,
        slot: 6,
    },
];

const TEXTS: &[&str] = &[
    "Did the build go through?",
    "Yes, it is green now.",
    "Can you look at it after lunch?",
    "Sure, I will take it.",
    "Thanks, that helps a lot.",
    "See you at the meeting.",
    "I moved the notes to the shared folder.",
    "Great, I will read them tonight.",
];

fn user(account: u64, n: u64) -> Result<UserId, AppError> {
    UserId::parse(&format!(
        "aci:00000000-0000-4000-8000-{:012x}",
        0xf3_0000 + account * 0x100 + n
    ))
    .map_err(|e| AppError::Fixture(e.to_string()))
}

/// The newest message time of each chat of the script, newest first.
fn chat_ends(script: &Script) -> Vec<i64> {
    let mut ends: std::collections::HashMap<ConvKey, i64> = std::collections::HashMap::new();
    for e in script.deliveries().iter().flatten() {
        if let BackendEvent::Message(m) = e {
            let at = ends.entry(m.conv.clone()).or_insert(m.sent_at);
            *at = (*at).max(m.sent_at);
        }
    }
    let mut ends: Vec<i64> = ends.into_values().collect();
    ends.sort_unstable_by(|a, b| b.cmp(a));
    ends
}

/// When the chat of `slot` ends: between the `slot + 2`th and `slot + 3`th chat of the fixture
/// account, newest first (the two newest are the showcase chats), so every extra chat stands
/// among the fixture's, older than its newest message.
fn slot_time(ends: &[i64], slot: usize) -> i64 {
    match (ends.get(slot + 2), ends.get(slot + 3)) {
        (Some(a), Some(b)) => b + (a - b) / 2,
        _ => ends.last().copied().unwrap_or(0) - 86_400_000 * i64::try_from(slot + 1).unwrap_or(1),
    }
}

/// A script of one account: its contacts, its chats and their messages.
fn script_of(
    name: &str,
    display_name: &str,
    account: u64,
    chats: &[Chat],
    ends: &[i64],
) -> Result<Script, AppError> {
    let me = user(account, 0)?;
    let mut script = Script::new(name, vec![me]);
    script.display_name = Some(display_name.into());
    let mut setup = Vec::new();
    let mut messages = Vec::new();
    for (ci, chat) in chats.iter().enumerate() {
        let members: Vec<UserId> = chat
            .people
            .iter()
            .map(|(_, n)| user(account, *n))
            .collect::<Result<_, _>>()?;
        for ((name, _), id) in chat.people.iter().zip(&members) {
            let mut p = ContactPatch::new(id.clone());
            p.profile_name = Patch::Set((*name).into());
            setup.push(BackendEvent::ContactUpsert(p));
        }
        let key = match chat.group {
            Some(_) => ConvKey::Group(format!("gv2:ZXh0cmEtZ3JvdXAt{account:03}").into()),
            None => ConvKey::Direct(members[0].clone()),
        };
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(signal_features(chat.group.is_some()));
        if let Some(title) = chat.group {
            patch.title = Patch::Set(title.into());
            let mut list: Vec<Member> = members
                .iter()
                .enumerate()
                .map(|(i, u)| Member {
                    user: u.clone(),
                    role: if i == 0 {
                        MemberRole::Admin
                    } else {
                        MemberRole::Member
                    },
                })
                .collect();
            list.push(Member {
                user: user(account, 0)?,
                role: MemberRole::Member,
            });
            patch.members = Some(MemberList::snapshot(list));
        }
        if chat.pinned {
            patch.pinned_order = Patch::Set(0);
        }
        setup.push(BackendEvent::ConversationUpsert(patch));
        // Four messages, four minutes apart, the last at the chat's date; the unread ones are
        // the newest and come from the other side.
        let end = slot_time(ends, chat.slot);
        let count = 4_usize;
        for i in 0..count {
            let at = end - 240_000 * i64::try_from(count - 1 - i).unwrap_or(0);
            let local = at.to_string();
            let text = TEXTS[(ci * 3 + i) % TEXTS.len()];
            let from_other = i % 2 == 0 || count - i <= chat.unread;
            let mut msg = if from_other {
                let author = &members[(ci + i) % members.len()];
                remote(&key, author, &local, at, text)
            } else {
                own(&key, &local, at, text)
            };
            msg.body = Body::text(text);
            msg.read = !(from_other && count - i <= chat.unread);
            messages.push(BackendEvent::Message(msg));
        }
    }
    for chunk in setup.chunks(50).chain(messages.chunks(50)) {
        script = script.deliver(chunk.to_vec());
    }
    Ok(script)
}

/// What the options add to the fixture data of `script` (`FixtureOptions::accounts`): the second
/// account of the test network from 2 accounts on, the registered WhatsApp account from 3.
pub(crate) fn extra_accounts(accounts: u8, script: &Script) -> Result<Option<Extra>, AppError> {
    if accounts < 2 {
        return Ok(None);
    }
    let ends = chat_ends(script);
    let second = script_of("second account", "Demo Work", 1, SECOND, &ends)?;
    let whatsapp = if accounts >= 3 {
        let wa = script_of("whatsapp account", "Demo Phone", 2, WHATSAPP, &ends)?;
        // Its chats state WhatsApp's features, as the fixture account does when it plays
        // WhatsApp (W5b, L8: the composer offers what WhatsApp's does).
        Some(FakeNetwork::playing(
            NetworkKind::WhatsApp,
            kourier_fake::scripts::as_whatsapp(wa),
        ))
    } else {
        None
    };
    Ok(Some(Extra { second, whatsapp }))
}

/// Links the extra accounts, one after the other, and waits for each one's data to be in the
/// core and acked. The second account of the test network is the NEXT account the plain fake
/// links (its script is pushed first); the WhatsApp account is the one account of its own fake.
pub(crate) async fn seed(
    core: &CoreHandle,
    fake: &FakeNetwork,
    extra: &Extra,
) -> Result<(), AppError> {
    use kourier_backend_api::Network as _;
    const TIMEOUT: Duration = Duration::from_secs(120);
    let control = fake.control();
    let before = control.finished().len();
    control.push_script(extra.second.clone());
    core.send(UiCommand::AddAccount {
        network: fake.kind(),
        flow: "qr".into(),
    })?;
    tokio::time::timeout(TIMEOUT, control.wait_for(|c| c.finished().len() > before))
        .await
        .map_err(|_| {
            AppError::Fixture("the second account's data did not arrive in time".into())
        })?;
    if let Some(wa) = &extra.whatsapp {
        let control = wa.control();
        core.send(UiCommand::AddAccount {
            network: wa.kind(),
            flow: "qr".into(),
        })?;
        tokio::time::timeout(TIMEOUT, control.wait_for(|c| !c.finished().is_empty()))
            .await
            .map_err(|_| {
                AppError::Fixture("the WhatsApp account's data did not arrive in time".into())
            })?;
    }
    Ok(())
}
