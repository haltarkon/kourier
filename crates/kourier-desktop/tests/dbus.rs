//! D-Bus tests on a private `dbus-daemon` (never the user's session bus): the notification
//! client against a mock notification server, single instance with two connections, and the
//! tray against a mock StatusNotifierWatcher.
//!
//! `harness = false`: ksni connects through `DBUS_SESSION_BUS_ADDRESS`, which must point at the
//! private bus before any thread starts.

use std::time::Duration;

use kourier_desktop::instance::{self, Claim, InstanceRequest};
use kourier_desktop::notifications::{
    self, CloseReason, ImageData, InlineReply, Notification, NotificationClient, NotificationEvent,
    Urgency,
};
use kourier_desktop::testing::{MockNotifications, MockWatcher, PLASMA_CAPS, PrivateBus};
use kourier_desktop::tray::{Tray, TrayConfig, TrayEvent, TrayState};
use kourier_desktop::{APP_ID, DesktopError};
use tokio::sync::mpsc;
use zbus::zvariant::{OwnedValue, Structure};

const WAIT: Duration = Duration::from_secs(5);

type TestResult = Result<(), String>;
type TestFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = TestResult> + 'a>>;
type TestFn = fn(&PrivateBus) -> TestFuture<'_>;

fn check(cond: bool, what: &str) -> TestResult {
    if cond { Ok(()) } else { Err(what.to_owned()) }
}

fn err(e: DesktopError) -> String {
    e.to_string()
}

async fn next<T>(rx: &mut mpsc::UnboundedReceiver<T>, what: &str) -> Result<T, String> {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .map_err(|_| format!("timed out waiting for {what}"))?
        .ok_or_else(|| format!("channel closed waiting for {what}"))
}

fn message(replaces: u32) -> Notification {
    Notification {
        replaces_id: replaces,
        app_name: "Kourier".into(),
        summary: "Olena".into(),
        body: "<b>hi</b> & bye".into(),
        default_action: true,
        inline_reply: Some(InlineReply {
            placeholder: "Reply…".into(),
            submit_label: String::new(),
        }),
        desktop_entry: Some(APP_ID.into()),
        category: Some("im.received".into()),
        origin_name: Some("Work".into()),
        image: ImageData::rgba(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 128]),
        urgency: Urgency::Normal,
        timeout_ms: -1,
        ..Notification::default()
    }
}

async fn notifications(bus: &PrivateBus) -> TestResult {
    let client_bus = bus.connect().await.map_err(err)?;
    // No server yet: a clear error, no auto-activation.
    match NotificationClient::connect(&client_bus).await {
        Err(DesktopError::NoService(_)) => {}
        other => return Err(format!("expected NoService, got {other:?}")),
    }
    // A server that appears later (plasmashell after an autostarted Kourier) is announced.
    let mut appeared = notifications::watch_server(&client_bus)
        .await
        .map_err(err)?;
    let server = MockNotifications::start(bus.address(), &PLASMA_CAPS)
        .await
        .map_err(err)?;
    let owner = next(&mut appeared, "the server's appearance").await?;
    let client = NotificationClient::connect(&client_bus)
        .await
        .map_err(err)?;
    check(
        client.owner() == Some(owner.as_str()),
        "the client knows the server it connected to",
    )?;
    check(
        client.capabilities().inline_reply(),
        "inline-reply capability",
    )?;
    let mut events = client.subscribe().await.map_err(err)?;

    let id = client.notify(&message(0)).await.map_err(err)?;
    check(id != 0, "an id")?;
    let call = server
        .with_log(|l| l.calls.last().cloned())
        .ok_or("no call")?;
    check(
        call.app_name == "Kourier" && call.summary == "Olena",
        "app name and summary",
    )?;
    check(
        call.body == "&lt;b&gt;hi&lt;/b&gt; &amp; bye",
        "the body is escaped for body-markup servers",
    )?;
    check(call.has_action("default"), "default action")?;
    check(call.has_action("inline-reply"), "inline-reply action")?;
    check(
        call.hint_str("desktop-entry").as_deref() == Some(APP_ID),
        "desktop-entry",
    )?;
    check(
        call.hint_str("category").as_deref() == Some("im.received"),
        "category",
    )?;
    check(
        call.hint_str("x-kde-origin-name").as_deref() == Some("Work"),
        "origin",
    )?;
    check(
        call.hint_str("x-kde-reply-placeholder-text").as_deref() == Some("Reply…"),
        "reply placeholder",
    )?;
    check(
        call.hints.get("urgency").and_then(|v| u8::try_from(v).ok()) == Some(1),
        "urgency",
    )?;
    let image = call.hints.get("image-data").ok_or("image-data")?;
    let image: Structure<'_> = Structure::try_from(image.try_clone().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let fields = image.fields();
    check(fields.len() == 7, "image-data has 7 fields")?;
    let width = i32::try_from(&fields[0]).map_err(|e| e.to_string())?;
    let rowstride = i32::try_from(&fields[2]).map_err(|e| e.to_string())?;
    let alpha = bool::try_from(&fields[3]).map_err(|e| e.to_string())?;
    let data: Vec<u8> = Vec::try_from(fields[6].try_clone().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    check(
        width == 2 && rowstride == 8 && alpha && data.len() == 8,
        "image-data values",
    )?;
    check(
        image.signature().to_string() == "(iiibiiay)",
        "image-data signature",
    )?;

    // Replacing keeps the id.
    let again = client.notify(&message(id)).await.map_err(err)?;
    check(again == id, "replaces_id keeps the notification")?;

    // A click: token first, then the action.
    server
        .invoke(id, "default", Some("tok-1"))
        .await
        .map_err(err)?;
    match next(&mut events, "activation token").await? {
        NotificationEvent::ActivationToken { id: i, token } if i == id && token == "tok-1" => {}
        other => return Err(format!("expected the token, got {other:?}")),
    }
    match next(&mut events, "action").await? {
        NotificationEvent::Action { id: i, key } if i == id && key == "default" => {}
        other => return Err(format!("expected the action, got {other:?}")),
    }
    server.reply(id, "on my way").await.map_err(err)?;
    match next(&mut events, "reply").await? {
        NotificationEvent::Replied { id: i, text } if i == id && text == "on my way" => {}
        other => return Err(format!("expected the reply, got {other:?}")),
    }
    client.close(id).await.map_err(err)?;
    match next(&mut events, "closed").await? {
        NotificationEvent::Closed {
            id: i,
            reason: CloseReason::Closed,
        } if i == id => {}
        other => return Err(format!("expected closed, got {other:?}")),
    }
    check(
        server.with_log(|l| l.closed == vec![id]),
        "CloseNotification recorded",
    )?;

    Ok(())
}

async fn plain_server(bus: &PrivateBus) -> TestResult {
    let server = MockNotifications::start(bus.address(), &["body", "actions"])
        .await
        .map_err(err)?;
    let client_bus = bus.connect().await.map_err(err)?;
    let client = NotificationClient::connect(&client_bus)
        .await
        .map_err(err)?;
    client.notify(&message(0)).await.map_err(err)?;
    let call = server
        .with_log(|l| l.calls.last().cloned())
        .ok_or("no call")?;
    check(call.body == "<b>hi</b> & bye", "plain body unescaped")?;
    check(
        !call.has_action("inline-reply"),
        "no inline reply without the capability",
    )?;
    check(call.has_action("default"), "default action")?;
    Ok(())
}

async fn single_instance(bus: &PrivateBus) -> TestResult {
    let name = "app.kourier.Kourier.test";
    let first = bus.connect().await.map_err(err)?;
    let mut primary = match instance::claim(first, name).await.map_err(err)? {
        Claim::Primary(p) => p,
        Claim::Secondary(_) => return Err("the first claim must win".into()),
    };
    let mut requests = primary.take_requests().ok_or("requests")?;
    let second = bus.connect().await.map_err(err)?;
    let second = match instance::claim(second, name).await.map_err(err)? {
        Claim::Secondary(bus) => bus,
        Claim::Primary(_) => return Err("the second claim must lose".into()),
    };
    instance::forward(
        &second,
        name,
        &InstanceRequest::Activate {
            token: Some("xdg-token".into()),
        },
    )
    .await
    .map_err(err)?;
    match next(&mut requests, "activate").await? {
        InstanceRequest::Activate { token } if token.as_deref() == Some("xdg-token") => {}
        other => return Err(format!("expected Activate, got {other:?}")),
    }
    instance::forward(
        &second,
        name,
        &InstanceRequest::Open {
            uris: vec!["sgnl://signal.me/#p/+15550001".into()],
            token: None,
        },
    )
    .await
    .map_err(err)?;
    match next(&mut requests, "open").await? {
        InstanceRequest::Open { uris, token } if uris.len() == 1 && token.is_none() => {}
        other => return Err(format!("expected Open, got {other:?}")),
    }
    // The name is free again once the first instance is gone.
    drop(requests);
    drop(primary);
    let third = bus.connect().await.map_err(err)?;
    let mut owned = false;
    for _ in 0..50 {
        if let Claim::Primary(_) = instance::claim(third.clone(), name).await.map_err(err)? {
            owned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check(owned, "the name is released with its connection")?;
    check(
        instance::object_path("app.kourier.Kourier.p1a2b-c") == "/app/kourier/Kourier/p1a2b_c",
        "object path",
    )
}

async fn tray(bus: &PrivateBus) -> TestResult {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = TrayConfig {
        id: APP_ID.into(),
        title: "Kourier".into(),
        icon_name: APP_ID.into(),
        show_label: "Show Kourier".into(),
        quit_label: "Quit".into(),
        ..TrayConfig::default()
    };
    // No watcher: no tray.
    match Tray::spawn(config.clone(), tx.clone()).await {
        Err(DesktopError::NoTray(_)) => {}
        Ok(_) => return Err("a tray without a watcher".into()),
        Err(e) => return Err(format!("expected NoTray, got {e}")),
    }
    let watcher = MockWatcher::start(bus.address()).await.map_err(err)?;
    let tray = Tray::spawn(config, tx).await.map_err(err)?;
    let item = watcher
        .wait_for_item(WAIT)
        .await
        .ok_or("no item registered")?;
    let sni = "org.kde.StatusNotifierItem";
    check(
        watcher.item_property(&item, "Status").await.map_err(err)? == "Active",
        "status Active",
    )?;
    check(
        watcher.item_property(&item, "Id").await.map_err(err)? == APP_ID,
        "item id",
    )?;
    tray.set_state(TrayState {
        unread: 3,
        tooltip: "3 unread".into(),
    })
    .await;
    check(
        watcher.item_property(&item, "Status").await.map_err(err)? == "NeedsAttention",
        "attention while unread",
    )?;
    // A click with an activation token.
    watcher
        .call_item(
            &item,
            "/StatusNotifierItem",
            sni,
            "ProvideXdgActivationToken",
            &("tray-tok",),
        )
        .await
        .map_err(err)?;
    watcher
        .call_item(&item, "/StatusNotifierItem", sni, "Activate", &(0i32, 0i32))
        .await
        .map_err(err)?;
    match next(&mut rx, "activate").await? {
        TrayEvent::Activate { token } if token.as_deref() == Some("tray-tok") => {}
        other => return Err(format!("expected Activate, got {other:?}")),
    }
    // Menu: Show (id 1), separator (2), Quit (3).
    let menu = "com.canonical.dbusmenu";
    let empty = OwnedValue::from(0i32);
    watcher
        .call_item(
            &item,
            "/MenuBar",
            menu,
            "Event",
            &(1i32, "clicked", &empty, 0u32),
        )
        .await
        .map_err(err)?;
    match next(&mut rx, "show").await? {
        TrayEvent::Show { token: None } => {}
        other => return Err(format!("expected Show, got {other:?}")),
    }
    watcher
        .call_item(
            &item,
            "/MenuBar",
            menu,
            "Event",
            &(3i32, "clicked", &empty, 0u32),
        )
        .await
        .map_err(err)?;
    match next(&mut rx, "quit").await? {
        TrayEvent::Quit => {}
        other => return Err(format!("expected Quit, got {other:?}")),
    }
    tray.shutdown().await;
    Ok(())
}

/// The one test this binary is, for cargo-nextest: it lists a binary with `--list` and runs a
/// test as `--exact <name> --nocapture`, which a `harness = false` binary does not know.
const LISTED_AS: &str = "dbus";

/// Answers `--list` (nextest's listing) with [`LISTED_AS`] and exits; any other arguments
/// return and `main` runs every case.
fn answer_list() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--list") {
        return;
    }
    // `--ignored` asks for the ignored tests only; this binary has none.
    if !args.iter().any(|a| a == "--ignored") {
        println!("{LISTED_AS}: test");
    }
    std::process::exit(0);
}

fn main() {
    answer_list();
    let bus = match PrivateBus::start() {
        Ok(b) => b,
        Err(e) => {
            println!("test dbus ... FAILED: cannot start a private dbus-daemon: {e}");
            std::process::exit(1);
        }
    };
    // SAFETY: no other thread exists yet (the runtime is built below).
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", bus.address());
        std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS");
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            println!("test dbus ... FAILED: runtime: {e}");
            std::process::exit(1);
        }
    };
    // A name by hand narrows the run to the cases it is part of; nextest names the whole
    // binary ([`LISTED_AS`]), which narrows nothing.
    let filter: Option<String> = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-') && a != LISTED_AS);
    let tests: [(&str, TestFn); 4] = [
        ("notifications", |b| Box::pin(notifications(b))),
        ("notifications_plain_server", |b| Box::pin(plain_server(b))),
        ("single_instance", |b| Box::pin(single_instance(b))),
        ("tray", |b| Box::pin(tray(b))),
    ];
    let mut failed = 0;
    let mut passed = 0;
    for (name, test) in tests {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let result = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), test(&bus))
                .await
                .unwrap_or_else(|_| Err("timed out".into()))
        });
        match result {
            Ok(()) => {
                passed += 1;
                println!("test {name} ... ok");
            }
            Err(e) => {
                failed += 1;
                println!("test {name} ... FAILED: {e}");
            }
        }
    }
    println!(
        "\ntest result: {}. {passed} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" }
    );
    drop(runtime);
    drop(bus);
    if failed > 0 {
        std::process::exit(1);
    }
}
