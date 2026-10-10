//! A private desktop session for trying Kourier's notifications and tray without touching the
//! real one: a `dbus-daemon` of its own with a mock notification server (Plasma's
//! capabilities) and a mock StatusNotifierWatcher.
//!
//! ```text
//! cargo run -p kourier-desktop --features testing --example mock_session
//! # prints: DBUS_SESSION_BUS_ADDRESS=unix:path=…
//! DBUS_SESSION_BUS_ADDRESS=… KOURIER_DESKTOP=1 kourier …
//! ```
//!
//! It prints every notification call (ids, hints and sizes; texts only with `--show-text`) and
//! the tray item, and reads commands from stdin:
//! `click <id> [token]`, `reply <id> <text>`, `dismiss <id>`, `tray-click [token]`,
//! `tray-show`, `tray-quit`, `quit`.

use std::io::BufRead as _;
use std::time::Duration;

use kourier_desktop::testing::{MockNotifications, MockWatcher, PLASMA_CAPS, PrivateBus};
use zbus::zvariant::OwnedValue;

const SNI: &str = "org.kde.StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let show_text = std::env::args().any(|a| a == "--show-text");
    let bus = PrivateBus::start()?;
    let notifications = MockNotifications::start(bus.address(), &PLASMA_CAPS).await?;
    let watcher = MockWatcher::start(bus.address()).await?;
    println!("DBUS_SESSION_BUS_ADDRESS={}", bus.address());

    // Print what arrives.
    let log = notifications.clone();
    tokio::spawn(async move {
        let mut seen = 0;
        let mut closed = 0;
        loop {
            log.wait_for(Duration::from_secs(3600), |l| {
                l.calls.len() > seen || l.closed.len() > closed
            })
            .await;
            let (calls, closes) = log.with_log(|l| (l.calls.clone(), l.closed.clone()));
            for c in &calls[seen..] {
                let mut hints: Vec<&str> = c.hints.keys().map(String::as_str).collect();
                hints.sort_unstable();
                println!(
                    "notify id={} replaces={} actions={:?} category={:?} origin={} hints={hints:?}",
                    c.id,
                    c.replaces_id,
                    c.actions
                        .chunks(2)
                        .filter_map(|a| a.first())
                        .collect::<Vec<_>>(),
                    c.hint_str("category"),
                    c.hint_str("x-kde-origin-name").is_some(),
                );
                if show_text {
                    println!("  summary: {}\n  body: {}", c.summary, c.body);
                } else {
                    println!(
                        "  summary: {} chars, body: {} chars",
                        c.summary.chars().count(),
                        c.body.chars().count()
                    );
                }
            }
            for id in &closes[closed..] {
                println!("close id={id}");
            }
            seen = calls.len();
            closed = closes.len();
        }
    });
    let w = watcher.clone();
    tokio::spawn(async move {
        if let Some(item) = w.wait_for_item(Duration::from_secs(3600)).await {
            println!("tray item registered: {item}");
        }
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    while let Some(line) = rx.recv().await {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or_default();
        let arg = words.next();
        let rest: Vec<&str> = words.collect();
        let id = || arg.and_then(|a| a.parse::<u32>().ok());
        let item = watcher.wait_for_item(Duration::from_millis(1)).await;
        let result = match (cmd, id(), item) {
            ("click", Some(id), _) => {
                notifications
                    .invoke(id, "default", rest.first().copied())
                    .await
            }
            ("reply", Some(id), _) => notifications.reply(id, &rest.join(" ")).await,
            ("dismiss", Some(id), _) => notifications.dismiss(id).await,
            ("tray-click", _, Some(item)) => {
                if let Some(token) = arg {
                    watcher
                        .call_item(
                            &item,
                            "/StatusNotifierItem",
                            SNI,
                            "ProvideXdgActivationToken",
                            &(token,),
                        )
                        .await?;
                }
                watcher
                    .call_item(&item, "/StatusNotifierItem", SNI, "Activate", &(0i32, 0i32))
                    .await
            }
            ("tray-show" | "tray-quit", _, Some(item)) => {
                let id: i32 = if cmd == "tray-show" { 1 } else { 3 };
                let data = OwnedValue::from(0i32);
                watcher
                    .call_item(
                        &item,
                        "/MenuBar",
                        MENU,
                        "Event",
                        &(id, "clicked", &data, 0u32),
                    )
                    .await
            }
            ("quit", _, _) => break,
            _ => {
                println!(
                    "? click <id> [token] | reply <id> <text> | dismiss <id> | tray-click [token] | tray-show | tray-quit | quit"
                );
                Ok(())
            }
        };
        if let Err(e) = result {
            println!("error: {e}");
        }
    }
    Ok(())
}
