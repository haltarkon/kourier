//! A group's invite link through the account's backend (`Query::GroupInvite`, the fake answers):
//! who is offered it, and how the link stands after each action.

use crate::support;

use kourier_core::facade::{Query, QueryData, UiCommand};
use kourier_fake::{Flavor, Script};
use kourier_model::{
    AccountUuid, ConversationId, GroupInviteAction as Action, GroupInvites, Member, MemberRole,
};
use support::send::*;
use support::{Env, WAIT, add_linked, eventually};

fn me_as(role: MemberRole) -> Script {
    let mut events = setup();
    events.push(conversation(
        group(),
        Some(vec![
            Member {
                user: alice(),
                role: MemberRole::Member,
            },
            Member {
                user: uid(ME),
                role,
            },
        ]),
    ));
    Script::new("group-invite", vec![uid(ME), uid(ME_PNI)]).deliver(events)
}

async fn linked(script: Script) -> (Env, AccountUuid, ConversationId) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let db = env.core_db();
    eventually("the group", WAIT, || {
        let db = db.clone();
        async move { conv_id(&db, &group()).is_some() }
    })
    .await;
    let id = conv_id(&db, &group()).expect("the group");
    (env, uuid, id)
}

/// The page's two facts about the link: what may be done with it, and whether it is on.
async fn page(env: &Env, id: ConversationId) -> Option<(GroupInvites, bool, bool)> {
    let QueryData::Group(detail) = env
        .handle
        .query(Query::Group(id))
        .await
        .expect("the query answers")
    else {
        panic!("another answer");
    };
    detail.map(|d| {
        let mine = d
            .members
            .iter()
            .any(|m| m.is_self && m.role == MemberRole::Admin);
        (d.invites, d.features.invite_link_on, mine)
    })
}

async fn ask(env: &Env, conversation: ConversationId, action: Action) -> Option<String> {
    let QueryData::GroupInvite(link) = env
        .handle
        .query(Query::GroupInvite {
            conversation,
            action,
        })
        .await
        .expect("the query answers")
    else {
        panic!("another answer");
    };
    link.map(|l| l.expose().clone())
}

async fn until_on(env: &Env, id: ConversationId, on: bool) {
    eventually("the link's state on the page", WAIT, || async move {
        page(env, id).await.is_some_and(|p| p.1 == on)
    })
    .await;
}

/// An administrator is offered the link, a plain member is not.
#[tokio::test]
async fn only_an_administrator_is_offered_the_link() {
    let (env, _, id) = linked(me_as(MemberRole::Admin)).await;
    eventually("the page of an administrator", WAIT, || {
        let env = &env;
        async move { page(env, id).await.is_some_and(|p| p.2) }
    })
    .await;
    assert_eq!(
        page(&env, id).await.expect("the page").0,
        GroupInvites::Switch
    );
    drop(env);

    let (env, _, id) = linked(me_as(MemberRole::Member)).await;
    eventually("the page of a member", WAIT, || {
        let env = &env;
        async move { page(env, id).await.is_some() }
    })
    .await;
    assert_eq!(
        page(&env, id).await.expect("the page").0,
        GroupInvites::None
    );
}

/// The link starts off; on brings it, a reset replaces it, off takes it away and keeps it, and
/// the page's state follows; every action reached the network.
#[tokio::test]
async fn each_action_leaves_the_link_as_it_says() {
    let (env, uuid, id) = linked(me_as(MemberRole::Admin)).await;
    assert_eq!(ask(&env, id, Action::Read).await, None);

    let first = ask(&env, id, Action::TurnOn).await.expect("the link is on");
    until_on(&env, id, true).await;
    assert_eq!(
        ask(&env, id, Action::Read).await.as_deref(),
        Some(first.as_str())
    );

    let second = ask(&env, id, Action::Reset).await.expect("a new link");
    assert_ne!(first, second, "a reset makes a new link");
    assert_eq!(
        ask(&env, id, Action::Read).await.as_deref(),
        Some(second.as_str())
    );

    assert_eq!(ask(&env, id, Action::TurnOff).await, None);
    until_on(&env, id, false).await;
    assert_eq!(ask(&env, id, Action::Read).await, None);

    let back = ask(&env, id, Action::TurnOn).await.expect("the link is on");
    assert_eq!(back, second, "turning on brings the same link back");
    until_on(&env, id, true).await;

    let asked = env.control.commands(uuid);
    assert_eq!(
        asked.iter().filter(|c| **c == "GroupInvite").count(),
        8,
        "every action asked the network: {asked:?}"
    );
}
