//! Key management against the session's real Secret Service: a fresh profile's master key,
//! one account, and the shred. Skipped without a session bus, a service, or when the default
//! collection is locked (never prompts). The profile name is unique per run and every item of
//! it is deleted again, whatever the outcome.

use kourier_app::keys::{APP_ID, KeyError, Keyring};
use kourier_desktop::secret::SecretService;
use kourier_model::{AccountUuid, NetworkKind};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn shred_in_the_real_secret_service() {
    let svc = match SecretService::connect(false).await {
        Ok(svc) => svc,
        Err(e) => {
            eprintln!("skipped: {e}");
            return;
        }
    };
    if !matches!(svc.is_locked().await, Ok(false)) {
        eprintln!("skipped: the default collection is locked or unreadable");
        return;
    }
    let profile = format!("selftest-{}", AccountUuid::new_random());
    let dir = tempfile::tempdir().unwrap();
    // The checks run in their own task, so a failing assertion cannot skip the cleanup below.
    let svc = Arc::new(svc);
    let outcome = tokio::spawn({
        let (svc, profile, root) = (svc.clone(), profile.clone(), dir.path().to_owned());
        async move { exercise(&svc, &profile, &root).await }
    })
    .await;
    let cleanup = svc
        .delete(&[("application", APP_ID), ("profile", &profile)])
        .await;
    let left = svc
        .lookup(&[("application", APP_ID), ("profile", &profile)])
        .await;
    outcome.expect("the checks panicked").unwrap();
    assert_eq!(
        cleanup.unwrap(),
        1,
        "only the master key was left to delete"
    );
    assert_eq!(left.unwrap(), None, "nothing left behind");
}

async fn exercise(
    svc: &SecretService,
    profile: &str,
    root: &std::path::Path,
) -> Result<(), KeyError> {
    let keys = Keyring::with_secret_service(svc, profile, true).await?;
    let again = Keyring::with_secret_service(svc, profile, false).await?;
    assert_eq!(*keys.core_db_key(), *again.core_db_key());

    let id = AccountUuid::new_random();
    let dir = root.join(id.to_string());
    let db_key = keys.create_account(&dir, id, NetworkKind::Signal).await?;
    assert_eq!(*again.open_account(&dir).await?.db_key, *db_key);

    keys.shred_account(&dir, id).await?;
    assert!(matches!(
        keys.open_account(&dir).await,
        Err(KeyError::AccountSecretMissing)
    ));
    Ok(())
}
