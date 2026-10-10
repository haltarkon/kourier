//! Group Send Endorsements on the send side (§6.7 step 5; ports
//! `SD/ts/util/groupSendEndorsements.preload.ts`: `validateGroupSendEndorsementsExpiration`,
//! `GroupSendEndorsementState.buildToken`, `maybeCreateGroupSendEndorsementState`).
//!
//! The apply zone stores the server's `GroupSendEndorsementsResponse` raw with its validated
//! expiry (S10). A send receives it again against the stored member list, which yields one
//! endorsement per member; the token for a set of recipients is their combined endorsement
//! turned into a `GroupSendFullToken` with the response's expiry. Endorsements are usable only
//! while their expiry is more than 2 hours and less than 2 days away; outside that window, or
//! when they do not match the member list, the sender refreshes the group first (once per send).

use std::time::Duration;

use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_core::{Aci, ServiceId};
use zkgroup::groups::{GroupSendEndorsement, GroupSendEndorsementsResponse};

use super::GroupRow;
use crate::error::Result;

/// Endorsements expiring sooner than this are not used (`TWO_HOURS`).
pub(crate) const MIN_REMAINING: Duration = Duration::from_secs(2 * 3600);
/// Endorsements expiring later than this are not trusted (`TWO_DAYS`).
pub(crate) const MAX_REMAINING: Duration = Duration::from_secs(2 * 24 * 3600);

/// Whether endorsements expiring at `expire_at` (ms) may be used at `now` (ms).
pub(crate) fn usable_expiry(expire_at: i64, now: i64) -> bool {
    let left = expire_at.saturating_sub(now);
    let min = i64::try_from(MIN_REMAINING.as_millis()).unwrap_or(i64::MAX);
    let max = i64::try_from(MAX_REMAINING.as_millis()).unwrap_or(i64::MAX);
    left > min && left < max
}

/// The endorsements of one group, one per member.
pub(crate) struct Endorsements {
    expiration: zkgroup::Timestamp,
    members: Vec<(Aci, GroupSendEndorsement)>,
    secret: zkgroup::groups::GroupSecretParams,
}

impl std::fmt::Debug for Endorsements {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endorsements")
            .field("members", &self.members.len())
            .field("expiration", &self.expiration.epoch_seconds())
            .finish()
    }
}

impl Endorsements {
    /// Whether `aci` has an endorsement.
    pub(crate) fn has(&self, aci: Aci) -> bool {
        self.members.iter().any(|(member, _)| *member == aci)
    }

    /// The serialized `GroupSendFullToken` for exactly `recipients` (each must have an
    /// endorsement); `None` when one has none.
    pub(crate) fn token(&self, recipients: &[Aci]) -> Option<Vec<u8>> {
        let mut parts = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            let (_, endorsement) = self.members.iter().find(|(m, _)| m == recipient)?;
            parts.push(*endorsement);
        }
        if parts.is_empty() {
            return None;
        }
        let combined = GroupSendEndorsement::combine(parts);
        let token = combined
            .to_token(self.secret)
            .into_full_token(self.expiration);
        Some(zkgroup::serialize(&token))
    }
}

/// The stored endorsements of `row`, when they are inside the usable window at `now` (ms) and
/// match its member list. `None` means: refresh the group (or send without them).
pub(crate) fn load(
    conn: &Connection,
    row: &GroupRow,
    server: &zkgroup::ServerPublicParams,
    now: i64,
) -> Result<Option<Endorsements>> {
    let Some(snapshot) = row.snapshot.as_ref() else {
        return Ok(None);
    };
    if !row
        .endorsements_expire_at
        .is_some_and(|at| usable_expiry(at, now))
    {
        return Ok(None);
    }
    let bytes: Option<Vec<u8>> = conn
        .prepare_cached("SELECT endorsements FROM groups WHERE master_key = ?1")?
        .query_row([&row.keys.master[..]], |r| r.get(0))
        .optional()?
        .flatten();
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let Ok(response) = zkgroup::deserialize::<GroupSendEndorsementsResponse>(&bytes) else {
        return Ok(None);
    };
    let expiration = response.expiration();
    let acis: Vec<Aci> = snapshot.members.iter().map(|m| Aci::from(m.aci)).collect();
    let seconds = u64::try_from(now / 1000).unwrap_or(0);
    let received = response.receive_with_service_ids_single_threaded(
        acis.iter().map(|aci| ServiceId::from(*aci)),
        zkgroup::Timestamp::from_epoch_seconds(seconds),
        &row.keys.secret,
        server,
    );
    match received {
        Ok(received) => Ok(Some(Endorsements {
            expiration,
            members: acis
                .into_iter()
                .zip(received.into_iter().map(|r| r.decompressed))
                .collect(),
            secret: row.keys.secret,
        })),
        Err(_) => {
            tracing::info!(
                group = %super::log_id(&row.keys.id),
                "stored send endorsements do not match the member list"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_two_hours_to_two_days() {
        let hour = 3_600_000;
        assert!(!usable_expiry(0, 0), "expired");
        assert!(!usable_expiry(2 * hour, 0), "expires within two hours");
        assert!(usable_expiry(2 * hour + 1, 0));
        assert!(usable_expiry(47 * hour, 0));
        assert!(!usable_expiry(48 * hour, 0), "too far in the future");
        assert!(!usable_expiry(i64::MIN, i64::MAX), "no overflow");
    }

    #[test]
    fn tokens_combine_the_recipients_endorsements() {
        use zkgroup::groups::{GroupMasterKey, GroupSecretParams, GroupSendDerivedKeyPair};
        let server = zkgroup::ServerSecretParams::generate([3; 32]);
        let public = server.get_public_params();
        let secret = GroupSecretParams::derive_from_master_key(GroupMasterKey::new([5; 32]));
        let acis: Vec<Aci> = (1..=3u128)
            .map(|n| Aci::from(uuid::Uuid::from_u128(n)))
            .collect();
        let now = zkgroup::Timestamp::from_epoch_seconds(1_700_000_000);
        let expiration = GroupSendEndorsementsResponse::default_expiration(now);
        let key = GroupSendDerivedKeyPair::for_expiration(expiration, &server);
        let ciphertexts: Vec<_> = acis
            .iter()
            .map(|aci| secret.encrypt_service_id(ServiceId::from(*aci)))
            .collect();
        let response = GroupSendEndorsementsResponse::issue(ciphertexts, &key, [9; 32]);
        let received = response
            .receive_with_service_ids_single_threaded(
                acis.iter().map(|a| ServiceId::from(*a)),
                now,
                &secret,
                &public,
            )
            .unwrap();
        let endorsements = Endorsements {
            expiration,
            members: acis
                .iter()
                .copied()
                .zip(received.into_iter().map(|r| r.decompressed))
                .collect(),
            secret,
        };
        assert!(endorsements.has(acis[0]));
        assert!(!endorsements.has(Aci::from(uuid::Uuid::from_u128(9))));
        let bytes = endorsements.token(&acis[1..]).expect("token");
        let token: zkgroup::groups::GroupSendFullToken = zkgroup::deserialize(&bytes).unwrap();
        token
            .verify(acis[1..].iter().map(|a| ServiceId::from(*a)), now, &key)
            .expect("the server accepts it for exactly those recipients");
        assert!(
            token
                .verify(acis.iter().map(|a| ServiceId::from(*a)), now, &key)
                .is_err(),
            "not for a different set"
        );
        assert!(
            endorsements
                .token(&[Aci::from(uuid::Uuid::from_u128(9))])
                .is_none()
        );
    }
}
