//! Column codecs: JSON columns, enums stored by variant name, `BackendBlob` bytes.

use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{
    AccountId, AccountState, BackendBlob, ConvKey, ConversationId, NetworkKind, UserId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{CoreError, Result};

/// Serializes `v` for a JSON column.
pub(crate) fn to_json<T: Serialize + ?Sized>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| CoreError::json(&e))
}

/// Parses a JSON column.
pub(crate) fn from_json<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| CoreError::json(&e))
}

/// Parses an optional JSON column.
pub(crate) fn from_json_opt<T: DeserializeOwned>(s: Option<String>) -> Result<Option<T>> {
    s.map(|s| from_json(&s)).transpose()
}

/// The serde name of a unit enum variant (`ReceiptKind::Read` → `"Read"`).
pub(crate) fn enum_name<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v).map_err(|e| CoreError::json(&e))? {
        serde_json::Value::String(s) => Ok(s),
        _ => Err(CoreError::Json("not a unit enum variant".into())),
    }
}

/// Parses a unit enum variant stored by name.
pub(crate) fn from_enum_name<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|e| CoreError::json(&e))
}

/// `BackendBlob` as stored in BLOB columns: 2-byte LE version, then the payload.
pub(crate) fn blob_to_sql(b: &BackendBlob) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.bytes.len() + 2);
    out.extend_from_slice(&b.v.to_le_bytes());
    out.extend_from_slice(&b.bytes);
    out
}

/// Inverse of [`blob_to_sql`].
pub(crate) fn blob_from_sql(bytes: Option<Vec<u8>>) -> Option<BackendBlob> {
    let bytes = bytes?;
    let (v, rest) = bytes.split_first_chunk::<2>()?;
    Some(BackendBlob::new(u16::from_le_bytes(*v), rest.to_vec()))
}

/// The stored name of an account state; the `LoggedOut` reason goes in `state_reason`.
pub(crate) fn account_state_name(s: &AccountState) -> (&'static str, Option<&str>) {
    match s {
        AccountState::Linking => ("Linking", None),
        AccountState::Active => ("Active", None),
        AccountState::LoggedOut { reason } => ("LoggedOut", Some(reason.as_str())),
        AccountState::Removing => ("Removing", None),
    }
}

/// Inverse of [`account_state_name`].
pub(crate) fn account_state(name: &str, reason: Option<String>) -> Result<AccountState> {
    Ok(match name {
        "Linking" => AccountState::Linking,
        "Active" => AccountState::Active,
        "LoggedOut" => AccountState::LoggedOut {
            reason: reason.unwrap_or_default(),
        },
        "Removing" => AccountState::Removing,
        _ => return Err(CoreError::Json("unknown account state".into())),
    })
}

/// Parses a stored user id.
pub(crate) fn user(s: &str) -> Result<UserId> {
    UserId::parse(s).map_err(|_| CoreError::Json("malformed stored user id".into()))
}

/// Parses a stored conversation key.
pub(crate) fn conv_key(s: &str) -> Result<ConvKey> {
    ConvKey::parse(s).map_err(|_| CoreError::Json("malformed stored conversation key".into()))
}

/// Parses a stored network name.
pub(crate) fn network(s: &str) -> Result<NetworkKind> {
    s.parse()
        .map_err(|_| CoreError::Json("unknown stored network".into()))
}

/// `u32` from a stored integer, clamped.
pub(crate) fn u32_of(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// Stored `i64` from a `u64`, clamped.
pub(crate) fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The account row fields the ingest and supervisor need.
#[derive(Clone, Debug)]
pub(crate) struct AccountInfo {
    pub id: AccountId,
    pub state: AccountState,
    pub own_ids: Vec<UserId>,
    pub local_notifications: bool,
    pub local_muted_until: Option<i64>,
    pub read_receipts: bool,
    pub typing_indicators: bool,
}

/// Loads [`AccountInfo`].
pub(crate) fn account_info(conn: &Connection, id: AccountId) -> Result<Option<AccountInfo>> {
    let row = conn
        .query_row(
            "SELECT network, state, state_reason, own_ids, local_prefs, settings
             FROM accounts WHERE id = ?1",
            [id.0],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((_network, state, reason, own, prefs, settings)) = row else {
        return Ok(None);
    };
    let prefs: kourier_model::AccountLocalPrefs = from_json(&prefs)?;
    let settings: kourier_model::AccountSettings = from_json(&settings)?;
    Ok(Some(AccountInfo {
        id,
        state: account_state(&state, reason)?,
        own_ids: from_json(&own)?,
        local_notifications: prefs.notifications,
        local_muted_until: prefs.muted_until,
        read_receipts: settings.read_receipts,
        typing_indicators: settings.typing_indicators,
    }))
}

/// The conversation id of `key` in `account`, if it exists.
pub(crate) fn conversation_id(
    conn: &Connection,
    account: AccountId,
    key: &ConvKey,
) -> Result<Option<ConversationId>> {
    Ok(conn
        .prepare_cached("SELECT id FROM conversations WHERE account_id = ?1 AND conv_key = ?2")?
        .query_row((account.0, key.encode()), |r| r.get(0).map(ConversationId))
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::{ReadState, ReceiptKind};

    #[test]
    fn enums_by_name_and_blobs_roundtrip() {
        assert_eq!(enum_name(&ReceiptKind::Viewed).unwrap(), "Viewed");
        assert_eq!(
            from_enum_name::<ReadState>("Read").unwrap(),
            ReadState::Read
        );
        assert!(enum_name(&kourier_model::OutgoingState::Sent { server_at: None }).is_err());
        let b = BackendBlob::new(7, vec![1, 2, 3]);
        assert_eq!(blob_from_sql(Some(blob_to_sql(&b))), Some(b));
        assert_eq!(blob_from_sql(Some(vec![1])), None);
        for s in [
            AccountState::Linking,
            AccountState::Active,
            AccountState::LoggedOut { reason: "x".into() },
            AccountState::Removing,
        ] {
            let (name, reason) = account_state_name(&s);
            assert_eq!(account_state(name, reason.map(str::to_owned)).unwrap(), s);
        }
    }
}
