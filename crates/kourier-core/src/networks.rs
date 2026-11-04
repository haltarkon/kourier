//! Accounts of a network this run does not have (K-A "paused accounts", decision 1 of
//! `target/next/KA-decisions.md`; architecture §4.3).
//!
//! There is no runtime switch in the core: the app registers an experimental network only while
//! the user has it on (a `kourier-app` preference read at the start), and an account whose
//! network is not registered is a PAUSED account. What the core does with it is today's
//! behaviour, written down here and pinned by `tests/networks.rs`:
//!
//! - its row, its chats and its messages stay and are read as any other account's; it is NOT
//!   wiped, logged out or marked failed (`reconcile` plans it, the start spawns no supervisor
//!   for it and logs "network not in this build; account not started");
//! - it has no actor, so its connection state is the default `Offline` (what `Query::Accounts`
//!   answers for an account without a report), a defined state that is not "needs action";
//! - nothing new is sent: every command that needs the network (`SendText`, an edit, a
//!   reaction, a delete, a retry) is refused with `UnknownNetwork`, and `AddAccount` of that
//!   network too. What was queued before the network went away stays in the outbox and goes
//!   out after the network is back and the account is online;
//! - `Unlink`, `Relink`, `RestartAccount` and `CancelLogin` find no supervisor and fail with
//!   `UnknownAccount`, leaving the row untouched (a front-end does not offer them while the
//!   network is off);
//! - `RemoveAccount` removes it locally ([`remove_without_network`]): the network cannot be
//!   asked to forget the device, so the outcome is `NotReached` (the front-end tells the user
//!   the phone keeps listing the device until it is removed there), or `NoDevice` for an
//!   account that was logged out. A `Removing` row found at a start whose network is off is
//!   finished the same way by `reconcile` ([`finish_removal`]: the same tail, which tells the
//!   outcome kept on the row, shred, directory, row, K-A fix C8); an `Unlinking` row found at a
//!   start whose network is off ends `LoggedOut` here ([`finish_unlink`]: chats kept, credentials
//!   wiped, the outcome kept on the row told).

use std::sync::Arc;

use kourier_db::Priority;
use kourier_model::{AccountId, AccountState, DeviceLogout, LogoutReport};

use crate::facade::{AccountLabel, UiEvent};
use crate::local::{self, AccountEntry};
use crate::runtime::{DERIVE_PARALLEL_PERMITS, Shared};
use crate::store::lane;

/// `RemoveAccount` for an account no supervisor exists for: when its network is not registered
/// in this run, starts the local removal and returns `true`; otherwise (no such account, or a
/// registered network whose supervisor is gone) it does nothing and returns `false`, and the
/// caller fails the command as it always did.
pub(crate) async fn remove_without_network(sh: &Arc<Shared>, account: AccountId) -> bool {
    let entries = match sh.store.read(local::accounts).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(account = account.0, error = %e, "reading the accounts failed; RemoveAccount not done");
            return false;
        }
    };
    let Some(entry) = entries.into_iter().find(|e| e.id == account) else {
        return false;
    };
    if sh.networks.get(entry.network).is_some() || sh.is_removing(account) {
        return false;
    }
    // Counted as being removed from here on, like the supervisor's removal (R1 fix 5): its
    // background work stops and a login right behind this command does not meet it.
    sh.removing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(account);
    tracing::info!(account = account.0, network = %entry.network, "removing an account of a network that is off");
    let sh = sh.clone();
    tokio::spawn(async move { remove(&sh, entry).await });
    true
}

/// A removal that a quit cut off, found at a start whose network is off (`reconcile`): the
/// same tail as [`remove_without_network`], so the outcome kept on the row is told too (K-A fix
/// C8; the plain discard of a `Removing` row dropped it silently).
pub(crate) fn finish_removal(sh: &Arc<Shared>, entry: AccountEntry) {
    if sh.is_removing(entry.id) {
        return;
    }
    sh.removing
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(entry.id);
    tracing::info!(account = entry.id.0, network = %entry.network, "finishing a removal of an account of a network that is off");
    let sh = sh.clone();
    tokio::spawn(async move { remove(&sh, entry).await });
}

/// An unlink that a quit cut off, found at a start whose network is off (`reconcile`): the
/// network cannot be asked to forget the device, so the account ends `LoggedOut` here with its
/// chats kept and its credentials wiped (the way of an unlink by the phone), and the user is
/// told what became of the device: the outcome the cut-off run kept on the row, else
/// `NotReached` (K-A fix C8; the row stayed `Unlinking` for good, no plan runs it).
pub(crate) fn finish_unlink(sh: &Arc<Shared>, entry: AccountEntry) {
    tracing::info!(account = entry.id.0, network = %entry.network, "finishing an unlink of an account of a network that is off");
    let sh = sh.clone();
    tokio::spawn(async move { unlink(&sh, entry).await });
}

/// The label the outcome notice names the account with.
async fn label(sh: &Arc<Shared>, entry: &AccountEntry) -> AccountLabel {
    let account = entry.id;
    match sh
        .store
        .read(move |c| local::account_label(c, account))
        .await
    {
        Ok(Some(who)) => who,
        _ => AccountLabel {
            network: entry.network,
            display_name: None,
            number: None,
            own_device: None,
            linked_at: 0,
            twin: false,
        },
    }
}

/// What the cut-off run kept on a `Removing` or `Unlinking` row (`None`: nothing kept).
async fn kept_report(sh: &Arc<Shared>, account: AccountId) -> Option<Option<LogoutReport>> {
    sh.store
        .read(move |c| local::removal_report(c, account))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(account = account.0, error = %e, "reading the kept removal outcome failed");
            None
        })
}

async fn unlink(sh: &Arc<Shared>, entry: AccountEntry) {
    let account = entry.id;
    let who = label(sh, &entry).await;
    let report = kept_report(sh, account)
        .await
        .flatten()
        .unwrap_or_else(|| LogoutReport::new(DeviceLogout::NotReached));
    // Kept before the credentials go, as the supervisor's unlink keeps it.
    let keep = report.clone();
    let _ = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::set_removal_report(c, account, Some(&keep))
        })
        .await;
    if let Err(e) = sh.vault.wipe_credentials(entry.uuid).await {
        tracing::warn!(account = account.0, error = %e, "wiping credentials failed");
    }
    let (now, reason) = (sh.now(), "logged out on request".to_owned());
    match sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::logged_out(c, account, &reason, now)
        })
        .await
    {
        Ok(ended) => crate::runtime::emit_changes(sh, &ended),
        Err(e) => {
            tracing::warn!(account = account.0, error = %e, "recording the logout failed; retried at the next start");
            return;
        }
    }
    tracing::info!(account = account.0, outcome = ?report.outcome, "unlinked an account of a network that is off");
    sh.emit(UiEvent::AccountsChanged);
    sh.emit_logout_outcome(account, false, report, who);
}

/// The tail of the supervisor's `remove` (§4.3 step 7) without the logout: the state
/// `Removing`, the outcome kept on the row before the shred, the shred, the row, the outcome
/// told, the files. A step that fails leaves the row `Removing`; the next start finishes it.
async fn remove(sh: &Arc<Shared>, entry: AccountEntry) {
    let account = entry.id;
    let who = label(sh, &entry).await;
    // A row a quit cut off has its outcome already (kept before the shred): it goes back with
    // the state and is the one told.
    let kept = if entry.state == AccountState::Removing {
        kept_report(sh, account).await
    } else {
        None
    };
    let keep_on_row = kept.clone();
    let _ = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::set_state_keeping_report(
                c,
                account,
                &AccountState::Removing,
                keep_on_row.as_ref(),
            )
        })
        .await;
    sh.emit(UiEvent::AccountsChanged);
    // A logged-out account has no device on the phone; any other is still listed there.
    let report = kept.flatten().unwrap_or_else(|| {
        LogoutReport::new(if matches!(entry.state, AccountState::LoggedOut { .. }) {
            DeviceLogout::NoDevice
        } else {
            DeviceLogout::NotReached
        })
    });
    // Kept before the shred, as the supervisor keeps it: a kill between the shred and the
    // row's deletion must not lose what to tell.
    let keep = report.clone();
    if let Err(e) = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::set_removal_report(c, account, Some(&keep))
        })
        .await
    {
        tracing::warn!(account = account.0, error = %e, "keeping the removal outcome failed");
    }
    if let Err(e) = sh.vault.shred(entry.uuid).await {
        tracing::warn!(account = account.0, error = %e, "shredding the account secret failed; removal retried at the next start");
        sh.emit(UiEvent::AccountError {
            account,
            detail: format!(
                "the account could not be removed completely ({e}); it is retried at the next start"
            ),
            restartable: false,
        });
        sh.emit(UiEvent::AccountsChanged);
        return;
    }
    if let Err(e) = sh
        .store
        .write(Priority::High, lane(account), move |c| {
            local::delete_account(c, account)
        })
        .await
    {
        tracing::warn!(account = account.0, error = %e, "deleting the account row failed; removal retried at the next start");
        sh.emit(UiEvent::AccountError {
            account,
            detail: format!(
                "the account could not be removed completely ({e}); it is retried at the next start"
            ),
            restartable: false,
        });
        sh.emit(UiEvent::AccountsChanged);
        return;
    }
    tracing::info!(account = account.0, outcome = ?report.outcome, "removed account of a network that is off");
    sh.emit_logout_outcome(account, true, report, who);
    // A thumbnail being computed for this account writes its file under the account's
    // directory: wait for every compute to end first (F6), as the supervisor does.
    let permits = sh
        .derive_permits
        .clone()
        .acquire_many_owned(DERIVE_PARALLEL_PERMITS)
        .await;
    if let Err(e) = sh.vault.remove_dir(entry.uuid).await {
        tracing::warn!(account = account.0, error = %e, "removing the account directory failed");
    }
    drop(permits);
    sh.emit(UiEvent::AccountRemoved { account });
    sh.emit(UiEvent::AccountsChanged);
}
