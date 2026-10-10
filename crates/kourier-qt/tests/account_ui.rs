//! Account states in the UI, on the real core over the fake network (S15 UI): the fixture
//! account ends offline, with a wrong clock, waiting for a challenge, or unlinked, and the
//! banner above the conversation list says so. "Link Again" relinks the same account (its
//! chats and messages stay, the banner goes, the composer works again); "Verify…" only asks
//! for the challenge's URL to be opened (no browser is started here) and the answer handed in
//! lifts the challenge. Each state runs in a process of its own (one Qt application per
//! process): the test binary starts itself once per state. Offscreen, `harness = false`.

use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, FixtureState, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

/// Where the fixture's challenge is "solved" (`kourier_fake::fixture::CHALLENGE_URL`); never
/// opened: `.invalid` does not resolve, and nothing in this test hands a URL to a browser.
const CHALLENGE_URL: &str = "https://challenge.invalid/kourier-fixture";

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

Item {
    id: test

    property string state: "STATE"
    property string step: "start"
    property int conversations: 0
    property real challengedAccount: 0
    property string challengeUrl: ""
    property int relinks: 0
    property int sentBefore: 0

    function fail(msg: string): void {
        console.error("FAIL [" + state + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function ownWith(text: string): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.own) {
                for (const b of r.blocks) {
                    if (b.type === "text" && b.plain === text) {
                        return true;
                    }
                }
            }
        }
        return false;
    }

    Instantiator {
        id: accounts
        model: App.accounts
        delegate: QtObject {
            required property real accountId
            required property string status
        }
    }
    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property bool own
            required property var blocks
            required property string receipt
        }
    }

    Connections {
        target: App
        function onChallengeRequested(account: real, url: string): void {
            test.challengedAccount = account;
            test.challengeUrl = url;
        }
        function onRelinkStarted(): void {
            test.relinks += 1;
        }
    }

    // The banner as the list page shows it.
    AccountBanner {
        id: banner
        width: 360
    }

    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                if (!App.loaded || !App.hasAccounts || accounts.count !== 1) return;
                if (App.conversations.count < 15) return;
                App.ready();
                test.conversations = App.conversations.count;
                test.step = test.state;
                return;

            // ---- offline: a quiet line, nothing to press ----
            case "offline":
                if (App.bannerKind !== "offline") return;
                if (App.bannerText !== "Offline. Messages are sent once the connection is back.") return fail(App.bannerText);
                if (App.bannerAction !== "" || !banner.visible || !banner.passing) return fail("offline is no call to act");
                if (accounts.objectAt(0).status !== "offline") return fail("the drawer's state dot");
                Qt.exit(0);
                return;

            // ---- a wrong clock: a warning that says by how much ----
            case "clock":
                if (App.bannerKind !== "clock") return;
                if (App.bannerText.indexOf("about 2 hours ahead") < 0) return fail(App.bannerText);
                if (App.bannerAction !== "" || banner.passing) return fail("the clock is set outside Kourier");
                Qt.exit(0);
                return;

            // ---- a challenge: the button asks for the browser, the answer lifts it ----
            case "challenge":
                if (App.bannerKind !== "challenge") return;
                if (App.bannerAction !== "Verify…") return fail("the banner's button: " + App.bannerAction);
                if (App.bannerAccount !== accounts.objectAt(0).accountId) return fail("the banner's account");
                App.bannerActivate();
                if (test.challengeUrl !== CHALLENGE || test.challengedAccount !== App.bannerAccount) return fail("the challenge to open: " + test.challengeUrl);
                // An empty answer is nothing to hand in.
                App.submitChallenge(test.challengedAccount, "  ");
                test.step = "challenge-empty";
                return;
            case "challenge-empty":
                if (App.bannerKind !== "challenge") return fail("an empty answer lifted the challenge");
                // What the browser handed back.
                App.submitChallenge(test.challengedAccount, "signalcaptcha://solved.by.the.test");
                test.step = "challenge-solved";
                return;
            case "challenge-solved":
                if (App.bannerKind !== "") return;
                if (accounts.objectAt(0).status !== "online") return fail("online once solved: " + accounts.objectAt(0).status);
                Qt.exit(0);
                return;

            // ---- unlinked: link again from the banner ----
            case "loggedout":
                if (App.bannerKind !== "loggedOut") return;
                if (App.bannerAction !== "Link Again") return fail("the banner's button: " + App.bannerAction);
                if (accounts.objectAt(0).status !== "loggedOut") return fail("the drawer's state dot");
                App.openConversation(CONVERSATION);
                test.step = "loggedout-chat";
                return;
            case "loggedout-chat":
                if (App.timeline.loading || rows.count < 5 || App.timeline.title === "") return;
                // The history is readable; the composer says why it cannot send.
                if (App.timeline.cannotSend.indexOf("link this device again") < 0) return fail("the composer's reason: " + App.timeline.cannotSend);
                // The banner names the state; the composer does not repeat its sentence.
                if (App.bannerText === "" || App.timeline.cannotSend === App.bannerText || App.timeline.cannotSend.indexOf(App.bannerText.split(".")[0]) >= 0) return fail("the same sentence twice: " + App.bannerText + " / " + App.timeline.cannotSend);
                App.bannerActivate();
                if (test.relinks !== 1) return fail("the link page was not asked for");
                // The link page knows the network: its icon and steps, and what "Try again"
                // starts a login on (the same account again, never a new one).
                if (App.login.network !== NETWORK) return fail("the link page's network: '" + App.login.network + "'");
                test.step = "relinking";
                return;
            case "relinking":
                if (App.login.kind === "failed") return fail("relink failed: " + App.login.error);
                if (App.login.kind !== "done") return;
                test.step = "relinked";
                return;
            case "relinked":
                if (App.bannerKind !== "" || accounts.objectAt(0).status !== "online") return;
                // The same account: its chats and the open history are still there.
                if (accounts.count !== 1) return fail("one account after linking again");
                if (App.conversations.count !== test.conversations) return fail("chats kept: " + App.conversations.count + " of " + test.conversations);
                if (App.timeline.cannotSend !== "") return;
                if (rows.count < 5) return fail("the open history stayed");
                if (!App.composer.send(CONVERSATION, "Back again")) return fail("send refused: " + App.composer.error);
                test.step = "sent";
                return;
            case "sent":
                if (!test.ownWith("Back again")) return;
                if (App.composer.notice !== "") return fail("notice: " + App.composer.notice);
                Qt.exit(0);
                return;
            }
        }
    }

    Timer {
        interval: 60000
        running: true
        onTriggered: fail("timeout; banner '" + App.bannerKind + "', account " + (accounts.count > 0 ? accounts.objectAt(0).status : "none") + ", login " + App.login.kind + ", cannot send '" + App.timeline.cannotSend + "', chats " + App.conversations.count + ", rows " + rows.count)
    }
}
"#;

/// Runs one state in this process; returns the exit code of the Qt event loop.
fn run(state: &str) -> i32 {
    let fixture_state = match state {
        "offline" => FixtureState::Offline,
        "clock" => FixtureState::ClockSkew,
        "challenge" => FixtureState::Challenge,
        "loggedout" => FixtureState::LoggedOut,
        other => panic!("unknown state {other}"),
    };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 12,
            state: fixture_state,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let conversation = handle
        .fixture()
        .and_then(|f| f.demo_conversation)
        .expect("a conversation");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = SCRIPT
        .replace("STATE", state)
        .replace("CHALLENGE", &format!("{CHALLENGE_URL:?}"))
        .replace("NETWORK", "\"Fake\"")
        .replace("CONVERSATION", &conversation.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/account_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    app.shutdown();
    code
}

fn main() {
    // Never on the user's desktop, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // A child: one state, then exit with the event loop's code.
    if let Ok(state) = std::env::var("KOURIER_ACCOUNT_UI_STATE") {
        std::process::exit(run(&state));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let mut failed = 0;
    let states = ["offline", "clock", "challenge", "loggedout"];
    for state in states {
        let status = Command::new(&exe)
            .env("KOURIER_ACCOUNT_UI_STATE", state)
            .status()
            .expect("run a state");
        if status.success() {
            println!("test account state {state} ... ok");
        } else {
            println!("test account state {state} ... FAILED ({status})");
            failed += 1;
        }
    }
    let passed = states.len() - failed;
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed");
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed");
        std::process::exit(1);
    }
}
