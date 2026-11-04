//! The account actor of TG-1a: it answers every command that carries a reply, a login says a typed
//! "not yet" and starts nothing, and the start rules refuse what they must (TG-1 plan §5.4 T-b3,
//! T-b4, §2.8).

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AbortAnswer, BackendEvent, Command, Exit, OutgoingKind, OutgoingRequest, StartMode,
    TransmitOutcome,
};
use kourier_model::{
    AccountSettingsPatch, Body, ContactChange, ConvKey, ConversationChange, DeviceLogout,
    ErrorKind, Identifier, IdentifierKind, LoginStepKind, MessageId, ProfilePatch, Redacted, Text,
    UserId,
};
use tokio::sync::oneshot;

use crate::support::{self, Running, TEST_KEY};

const WAIT: Duration = Duration::from_secs(5);

fn user() -> UserId {
    UserId::parse("tg:77").expect("a user id")
}

fn conv() -> ConvKey {
    ConvKey::Direct(user())
}

/// Awaits an answer: `Some` when it came, `None` when the actor dropped the reply.
async fn answered<T>(name: &str, rx: oneshot::Receiver<T>) -> Option<T> {
    match tokio::time::timeout(WAIT, rx).await {
        Ok(Ok(answer)) => Some(answer),
        Ok(Err(_)) => None,
        Err(_) => panic!("{name}: the actor did not answer in time"),
    }
}

fn is_unsupported<T>(answer: Option<Result<T, kourier_model::BackendError>>) -> bool {
    matches!(answer, Some(Err(e)) if e.kind == ErrorKind::Unsupported)
}

async fn shut_down(mut run: Running) {
    let _ = run.ends.commands.send(Command::Shutdown).await;
    let exit = tokio::time::timeout(WAIT, &mut run.handle.exited)
        .await
        .expect("the actor did not end");
    assert_eq!(Exit::from_channel(exit), Exit::Shutdown);
}

#[test]
fn every_command_with_a_reply_is_answered() {
    let dir = tempfile::tempdir().expect("tempdir");
    support::rt().block_on(async {
        let run = support::start(dir.path(), StartMode::Fresh, TEST_KEY);
        let send = |c: Command| {
            let commands = run.ends.commands.clone();
            async move { commands.send(c).await.expect("the actor takes commands") }
        };

        let (reply, rx) = oneshot::channel();
        let req = OutgoingRequest {
            conv: conv(),
            local: Arc::from("k0000000000000001"),
            sent_at: 5,
            kind: OutgoingKind::Message {
                body: Body::Text(Text::plain("x")),
                reply_to: None,
                expiry: None,
            },
            only: None,
            is_recipient_update: false,
        };
        send(Command::Transmit {
            local: MessageId(1),
            req: Box::new(req),
            reply,
        })
        .await;
        assert!(
            matches!(answered("Transmit", rx).await, Some(TransmitOutcome::Failed(e)) if e.kind == ErrorKind::Unsupported),
            "Transmit: reply dropped or not a typed failure"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::AbortTransmit {
            local: MessageId(1),
            reply,
        })
        .await;
        assert_eq!(
            answered("AbortTransmit", rx).await,
            Some(AbortAnswer::TooLate),
            "AbortTransmit: reply dropped or not TooLate"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::ChangeConversation {
            conv: conv(),
            change: ConversationChange::SetArchived { archived: true },
            id: None,
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("ChangeConversation", rx).await),
            "ChangeConversation: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::SetRoamingWrites { on: true, reply }).await;
        assert!(
            is_unsupported(answered("SetRoamingWrites", rx).await),
            "SetRoamingWrites: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::ChangeContact {
            user: user(),
            change: ContactChange::SetNickname { nickname: None },
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("ChangeContact", rx).await),
            "ChangeContact: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::ChangeAccountSettings {
            patch: AccountSettingsPatch::default(),
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("ChangeAccountSettings", rx).await),
            "ChangeAccountSettings: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::ChangeProfile {
            patch: ProfilePatch::default(),
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("ChangeProfile", rx).await),
            "ChangeProfile: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::Resolve {
            id: Identifier {
                kind: IdentifierKind::Username,
                value: "someone".to_owned(),
            },
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("Resolve", rx).await),
            "Resolve: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::ListOwnDevices { reply }).await;
        assert!(
            is_unsupported(answered("ListOwnDevices", rx).await),
            "ListOwnDevices: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::SubmitChallenge {
            token: None,
            answer: Redacted::new("x".to_owned()),
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("SubmitChallenge", rx).await),
            "SubmitChallenge: reply dropped"
        );

        // P5a: no safety numbers on Telegram; nothing pauses on a Telegram identity.
        let (reply, rx) = oneshot::channel();
        send(Command::SafetyNumber { user: user(), reply }).await;
        assert!(
            matches!(
                answered("SafetyNumber", rx).await,
                Some(Ok(Err(kourier_model::SafetyUnavailable::Unsupported)))
            ),
            "SafetyNumber: reply dropped or not Unsupported"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::SetVerified {
            user: user(),
            verified: true,
            key: kourier_model::IdentityFingerprint(vec![5; 33]),
            reply,
        })
        .await;
        assert!(
            is_unsupported(answered("SetVerified", rx).await),
            "SetVerified: reply dropped"
        );

        let (reply, rx) = oneshot::channel();
        send(Command::ConfirmIdentity {
            user: user(),
            key: kourier_model::IdentityFingerprint(vec![5; 33]),
            reply,
        })
        .await;
        assert!(
            matches!(answered("ConfirmIdentity", rx).await, Some(Ok(()))),
            "ConfirmIdentity: reply dropped or refused"
        );

        assert_eq!(run.helpers_made(), 0, "a command started a helper");
        shut_down(run).await;
    });
}

/// A10a: the core's own durable change of a chat (the clearing of a mark when a chat is read) has no
/// reply; it is confirmed with a durable `CommandPersisted` and dropped, so the core does not send it again
/// at every start, and no helper is started for it. Mutant: the arm confirms nothing.
#[test]
fn a_durable_conversation_change_is_confirmed_and_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    support::rt().block_on(async {
        let mut run = support::start(dir.path(), StartMode::Fresh, TEST_KEY);
        run.ends
            .commands
            .send(Command::ChangeConversationDurable {
                cmd: kourier_model::CmdId(41),
                conv: conv(),
                change: ConversationChange::SetMarkedUnread {
                    marked_unread: false,
                },
                asked_at: 1_700_000_000_000,
            })
            .await
            .expect("the actor takes commands");
        let envelope = tokio::time::timeout(WAIT, run.ends.events.recv())
            .await
            .expect("no confirmation in time")
            .expect("the actor's events ended");
        assert!(
            matches!(
                envelope.events.as_slice(),
                [BackendEvent::CommandPersisted { cmd }] if *cmd == kourier_model::CmdId(41)
            ),
            "the durable change was not confirmed with its own command id"
        );
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
        assert_eq!(run.helpers_made(), 0, "a command started a helper");
        shut_down(run).await;
    });
}

#[test]
fn start_login_says_not_yet_typed() {
    let dir = tempfile::tempdir().expect("tempdir");
    support::rt().block_on(async {
        let mut run = support::start(dir.path(), StartMode::Fresh, TEST_KEY);
        run.ends
            .commands
            .send(Command::StartLogin {
                flow: "qr".to_owned(),
                relink: None,
                taken: Vec::new(),
            })
            .await
            .expect("the actor takes commands");
        let envelope = tokio::time::timeout(WAIT, run.ends.events.recv())
            .await
            .expect("no login step in time")
            .expect("the actor's events ended");
        assert!(envelope.ack.is_none(), "the step is not ephemeral");
        assert_eq!(envelope.events.len(), 1, "not exactly one event");
        match &envelope.events[0] {
            BackendEvent::Login(step) => match &step.kind {
                LoginStepKind::Failed { error } => {
                    assert_eq!(error.kind, ErrorKind::Unsupported, "not a typed not-yet");
                    assert_eq!(step.step_id, "qr", "the failed step is not the flow's");
                }
                _ => panic!("the login step is not a failure"),
            },
            _ => panic!("the event is not a login step"),
        }
        // Nothing else follows, and nothing was started.
        let more = tokio::time::timeout(Duration::from_millis(300), run.ends.events.recv()).await;
        assert!(more.is_err(), "more than one event followed a login");
        assert_eq!(run.helpers_made(), 0, "a login started a helper");
        shut_down(run).await;
    });
}

#[test]
fn a_resume_without_its_file_is_refused_and_makes_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    support::rt().block_on(async {
        for start in [StartMode::Resume, StartMode::Rebuild] {
            let mut run = support::start(dir.path(), start, TEST_KEY);
            let exit = tokio::time::timeout(WAIT, &mut run.handle.exited)
                .await
                .expect("the actor did not end");
            assert!(
                matches!(Exit::from_channel(exit), Exit::Failed(_)),
                "a start without its file was not refused"
            );
            assert!(
                !dir.path().join("backend.db").exists(),
                "a refused start made the file"
            );
        }
    });
}

#[test]
fn a_wrong_key_fails_the_start_and_leaves_the_file_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("backend.db");
    support::rt().block_on(async {
        let run = support::start(dir.path(), StartMode::Fresh, TEST_KEY);
        shut_down(run).await;
        let before = std::fs::read(&path).expect("the file exists");
        let mut run = support::start(dir.path(), StartMode::Resume, [0x11; 32]);
        let exit = tokio::time::timeout(WAIT, &mut run.handle.exited)
            .await
            .expect("the actor did not end");
        assert!(
            matches!(Exit::from_channel(exit), Exit::Failed(_)),
            "a wrong key did not fail the start"
        );
        assert_eq!(
            std::fs::read(&path).expect("the file exists"),
            before,
            "a wrong key changed the file"
        );
        // The right key still resumes it.
        let run = support::start(dir.path(), StartMode::Resume, TEST_KEY);
        shut_down(run).await;
    });
}

/// F11: a logout of an account that never linked tells the core, exactly once, that there was no
/// device to remove, and only then exits `LoggedOut` (the core reads that report for a removal:
/// no report would be told as "could not be reached").
#[test]
fn a_logout_reports_no_device_exactly_once_before_the_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    support::rt().block_on(async {
        let mut run = support::start(dir.path(), StartMode::Fresh, TEST_KEY);
        run.ends
            .commands
            .send(Command::Logout)
            .await
            .expect("the actor takes commands");
        let exit = tokio::time::timeout(WAIT, &mut run.handle.exited)
            .await
            .expect("the actor did not end");
        assert!(
            matches!(Exit::from_channel(exit), Exit::LoggedOut { .. }),
            "a logout did not end LoggedOut"
        );
        // The exit comes after the report: everything the actor said is queued by now.
        let mut reports = Vec::new();
        let mut others = 0;
        while let Ok(envelope) = run.ends.events.try_recv() {
            assert!(envelope.ack.is_none(), "the report is not ephemeral");
            for event in envelope.events {
                match event {
                    BackendEvent::Logout(report) => reports.push(report.outcome),
                    _ => others += 1,
                }
            }
        }
        assert_eq!(reports.len(), 1, "not exactly one logout report");
        assert!(
            matches!(reports[0], DeviceLogout::NoDevice),
            "the report is not NoDevice"
        );
        assert_eq!(others, 0, "a logout said more than its report");
        assert_eq!(run.helpers_made(), 0, "a logout started a helper");
    });
}
