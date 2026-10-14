//! `MediaHandles::deriving`: a downloaded picture counts as "being derived" from before the UI
//! hears that it is local until the events of the group that recorded its thumbnail are out.
//! A screenshot (and any view that wants the final picture) waits for 0, because the thumbnail
//! changes the picture's URLs once more (its blurhash and dimensions come with it).

mod support;

use std::time::{Duration, Instant};

use kourier_backend_api::BackendEvent;
use kourier_core::facade::UiCommand;
use kourier_fake::attachments::synthetic_image;
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, Script};
use kourier_model::{Attachment, Body};
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{Env, WAIT, add_linked, eventually};

/// What the commit of a derive group publishes at least (a conversation and a message change).
const EVENTS_PER_GROUP: usize = 2;

/// Fewer than one derive group (`GROUP`, 16): one commit records them all.
const PICTURES: usize = 6;

fn script() -> Script {
    let pictures = (1..=PICTURES as i64)
        .map(|i| {
            let mut m = remote(&d_alice(), &alice(), &(T0 + i).to_string(), T0 + i, "");
            m.body = Body::Media {
                attachments: vec![synthetic_image(640, 480, u64::try_from(i).unwrap_or(1))],
                caption: None,
            };
            BackendEvent::Message(m)
        })
        .collect();
    Script::new("derive-count", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(pictures)
}

fn count(env: &Env, filter: &str) -> usize {
    env.peek(|c| {
        c.query_row(
            &format!("SELECT count(*) FROM attachments WHERE {filter}"),
            [],
            |r| r.get::<_, i64>(0),
        )
        .map_or(0, |n| usize::try_from(n).unwrap_or(0))
    })
}

async fn pictures(env: &Env) -> Vec<Attachment> {
    let conv = env.conversations().await.first().expect("conversation").id;
    env.timeline(conv)
        .await
        .into_iter()
        .filter_map(|row| match row.body {
            Body::Media { attachments, .. } => Some(attachments),
            _ => None,
        })
        .flatten()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_downloaded_picture_counts_as_deriving_until_its_thumbnail_is_published() {
    let mut env = Env::new(Flavor::SignalLike, script()).await;
    let media = env.handle.media();
    assert_eq!(media.deriving(), 0, "nothing is derived yet");
    // The derive tasks wait, so the time between "downloaded" and "thumbnail" is as long as
    // the test wants.
    media.hold_derives();
    env.handle.send(UiCommand::UiReady).expect("send");
    add_linked(&mut env).await;

    // Every picture is downloaded and told to the UI; every one is counted.
    eventually("every picture downloaded and counted", WAIT, || {
        let ready = count(&env, "file_id IS NOT NULL") == PICTURES && media.deriving() == PICTURES;
        async move { ready }
    })
    .await;
    // Held: the count stays, and the pictures are as downloaded (no blurhash, no thumbnail).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(media.deriving(), PICTURES, "a held job stopped counting");
    assert_eq!(count(&env, "derived_at IS NOT NULL"), 0);
    let before = pictures(&env).await;
    assert_eq!(before.len(), PICTURES);
    assert!(before.iter().all(|a| a.blurhash.is_none()), "{before:?}");

    // Let them go. A task releases its jobs after the events of the group it recorded are out
    // (each such commit publishes at least `ConversationsChanged` and `MessagesChanged`), so
    // when the count reads 0 every group's events are out. A thread that does nothing else
    // reads each drop of the count and how many events had been published by then.
    let tail = env.handle.subscribe();
    assert_eq!(tail.len(), 0);
    // (A second one nobody reads, to count what the first one saw.)
    let mut unread = env.handle.subscribe();
    let counter = media.clone();
    let sampler = std::thread::spawn(move || {
        let deadline = Instant::now() + WAIT;
        let (mut last, mut drops) = (PICTURES, 0);
        loop {
            let now = counter.deriving();
            if now < last {
                drops += 1;
                last = now;
            }
            if now == 0 {
                return Some((drops, tail.len()));
            }
            if Instant::now() > deadline {
                return None;
            }
            std::hint::spin_loop();
        }
    });
    media.release_derives();
    let (drops, events) = sampler
        .join()
        .expect("sampler")
        .expect("the count reached 0");
    assert!(
        events >= EVENTS_PER_GROUP * drops,
        "the count dropped {drops} times but only {events} UI events were out by the last drop: \
         a job was released before its group's events were published"
    );
    // The count is 0 now, but the front-end has not heard: the events wait for a subscriber
    // that does not read (`tail`), and `events_unseen` is what a screenshot adds to the count.
    let published = unread.len();
    assert!(published >= events, "{published} events on the tail");
    assert!(
        env.handle.events_unseen() >= published,
        "{} events unseen, {published} on a tail nobody reads",
        env.handle.events_unseen()
    );
    // Every event of a publish with derive jobs went out after its jobs were counted (the
    // probe sits in the supervisor's `emit`; there were such events).
    let (seen, unreserved) = media.reserve_probe();
    assert!(seen >= EVENTS_PER_GROUP, "the probe saw {seen} events");
    assert_eq!(
        unreserved, 0,
        "{unreserved} of {seen} UI events of publishes with derive jobs went out before the \
         jobs were counted"
    );
    // (What the other subscribers of the test's core read is theirs: no "0 once read".)
    while unread.try_recv().is_ok() {}
    // And the next timeline query carries what the thumbnails changed.
    assert_eq!(count(&env, "derived_at IS NOT NULL"), PICTURES);
    let after = pictures(&env).await;
    assert!(after.iter().all(|a| a.blurhash.is_some()), "{after:?}");
    env.stop().await;
}
