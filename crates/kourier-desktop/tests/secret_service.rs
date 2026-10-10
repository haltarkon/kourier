//! Round trip against the real Secret Service of the session (gnome-keyring or KWallet's
//! ksecretd). Skipped without a session bus, a service, or when the collection is locked (the
//! test never prompts). Items carry a self-test application attribute and a unique run id and
//! are deleted again, whatever the outcome.

use kourier_desktop::secret::{SecretError, SecretService};
use std::sync::Arc;

const APP: &str = "app.kourier.Kourier.selftest";

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn store_lookup_replace_delete() {
    let svc = match SecretService::connect(false).await {
        Ok(svc) => svc,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    match svc.is_locked().await {
        Ok(false) => {}
        Ok(true) => {
            eprintln!("skipped: the default collection is locked");
            return;
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    }
    // Leftovers of an interrupted earlier run.
    svc.delete(&[("application", APP)]).await.unwrap();

    let run = uuid::Uuid::new_v4().to_string();
    // The checks run in their own task, so a failing assertion cannot skip the cleanup below.
    let svc = Arc::new(svc);
    let outcome = tokio::spawn({
        let (svc, run) = (svc.clone(), run.clone());
        async move {
            let attrs = [
                ("application", APP),
                ("kind", "test"),
                ("test-run", run.as_str()),
            ];
            exercise(&svc, &attrs).await
        }
    })
    .await;
    let cleanup = svc.delete(&[("application", APP)]).await;
    let left = svc.lookup(&[("application", APP)]).await;
    outcome.expect("the checks panicked").unwrap();
    cleanup.unwrap();
    assert_eq!(left.unwrap(), None, "nothing left behind");
}

async fn exercise(svc: &SecretService, attrs: &[(&str, &str)]) -> Result<(), SecretError> {
    assert_eq!(svc.lookup(attrs).await?, None);
    svc.store("Kourier self-test", attrs, &[1; 32]).await?;
    assert_eq!(
        svc.lookup(attrs).await?.as_deref().map(Vec::as_slice),
        Some(&[1u8; 32][..])
    );
    svc.store("Kourier self-test", attrs, &[2; 32]).await?;
    assert_eq!(
        svc.lookup(attrs).await?.as_deref().map(Vec::as_slice),
        Some(&[2u8; 32][..])
    );
    assert_eq!(svc.delete(attrs).await?, 1, "replace kept one item");
    assert_eq!(svc.lookup(attrs).await?, None);
    Ok(())
}
