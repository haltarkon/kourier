//! Group auth credentials (`SD/ts/services/groupCredentialFetcher.preload.ts`,
//! `WebAPI.preload.ts` `getGroupCredentials`): one `AuthCredentialWithPni` per day for today and
//! the next six days, fetched on the authenticated socket and checked with
//! `AuthCredentialWithPniResponse::receive`. Every Storage Service group request presents
//! today's credential for that group: HTTP Basic auth `hex(GroupPublicParams) :
//! hex(presentation)` (`groups.preload.ts:4268-4293`).

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::HeaderValue;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use rand::Rng as _;
use serde::Deserialize;
use zeroize::Zeroizing;
use zkgroup::auth::{AuthCredentialWithPni, AuthCredentialWithPniResponse};

use super::GroupKeys;
use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::net::{ChatRequest, SignalTransport, parse_json};

/// Seconds per day.
const DAY_SECS: u64 = 86_400;
/// The window Desktop keeps: today and six more days.
const WINDOW_DAYS: u64 = 6;

/// One credential of `GET /v1/certificate/auth/group`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialBody {
    credential: String,
    redemption_time: u64,
}

/// The answer of `GET /v1/certificate/auth/group`.
#[derive(Deserialize)]
struct CredentialsBody {
    credentials: Vec<CredentialBody>,
    #[serde(default)]
    pni: Option<String>,
}

/// The start of the UTC day of `now_ms`, in seconds.
pub(crate) fn day_start(now_ms: u64) -> u64 {
    let secs = now_ms / 1000;
    secs - secs % DAY_SECS
}

fn stored(conn: &Connection, day: u64) -> Result<Option<Zeroizing<Vec<u8>>>> {
    Ok(conn
        .prepare_cached("SELECT credential FROM group_credentials WHERE redemption_time = ?1")?
        .query_row([i64::try_from(day).unwrap_or(i64::MAX)], |r| r.get(0))
        .optional()?
        .map(Zeroizing::new))
}

/// Forgets every stored credential (the server refused one: 401).
pub(crate) fn forget(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM group_credentials", [])?;
    Ok(())
}

/// Fetches the window starting today, checks every credential and stores them (replacing older
/// days).
async fn fetch_window<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
    account: &Account,
    server: &zkgroup::ServerPublicParams,
    today: u64,
) -> Result<()> {
    let end = today + WINDOW_DAYS * DAY_SECS;
    let path = format!(
        "/v1/certificate/auth/group?redemptionStartSeconds={today}&redemptionEndSeconds={end}\
         &v101=true&zkcCredential=true"
    );
    let response = shared.net.request(socket, ChatRequest::get(&path)?).await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "group credentials",
            status: response.status.as_u16(),
        });
    }
    let body: CredentialsBody = parse_json(&response)?;
    if let Some(pni) = &body.pni
        && pni.to_ascii_lowercase() != uuid::Uuid::from(account.pni).to_string()
    {
        // Desktop logs the same mismatch; the credentials below then fail to verify.
        tracing::warn!("group credentials name another PNI than ours");
    }
    let mut received = Vec::with_capacity(body.credentials.len());
    for credential in body.credentials {
        let bytes = crate::wire_b64::LENIENT
            .decode(credential.credential.as_bytes())
            .map_err(|_| Error::Decode("group credential"))?;
        let response = AuthCredentialWithPniResponse::new(&bytes)
            .map_err(|_| Error::Decode("group credential"))?;
        let day = credential.redemption_time;
        if !day.is_multiple_of(DAY_SECS) || day < today || day > end {
            return Err(Error::Decode("group credential redemption time"));
        }
        let credential = response
            .receive(
                server,
                account.aci,
                account.pni,
                zkgroup::Timestamp::from_epoch_seconds(day),
            )
            .map_err(|_| Error::Decode("group credential (does not verify)"))?;
        received.push((day, Zeroizing::new(zkgroup::serialize(&credential))));
    }
    let now = shared.now();
    shared.db.zone(|tx| -> Result<()> {
        tx.prepare_cached("DELETE FROM group_credentials WHERE redemption_time < ?1")?
            .execute([i64::try_from(today).unwrap_or(i64::MAX)])?;
        for (day, credential) in &received {
            tx.prepare_cached(
                "INSERT INTO group_credentials(redemption_time, credential, fetched_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(redemption_time) DO UPDATE SET credential = excluded.credential,
                   fetched_at = excluded.fetched_at",
            )?
            .execute((
                i64::try_from(*day).unwrap_or(i64::MAX),
                credential.as_slice(),
                now,
            ))?;
        }
        Ok(())
    })?;
    tracing::debug!(days = received.len(), "group credentials fetched");
    Ok(())
}

/// The `Authorization` header for a group request: today's credential presented for `keys`,
/// fetching the window when today's is missing.
pub(crate) async fn authorization<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
    account: &Account,
    keys: &GroupKeys,
) -> Result<HeaderValue> {
    let server = shared
        .zk_params
        .as_ref()
        .ok_or(Error::Internal("the environment has no zkgroup params"))?;
    let today = day_start(shared.now_ms());
    let credential = match shared.db.read(|c| stored(c, today))? {
        Some(credential) => credential,
        None => {
            fetch_window(shared, socket, account, server, today).await?;
            shared
                .db
                .read(|c| stored(c, today))?
                .ok_or(Error::Decode("group credentials without today's"))?
        }
    };
    let credential =
        AuthCredentialWithPni::new(&credential).map_err(|_| Error::Decode("group credential"))?;
    let mut randomness = [0u8; 32];
    rand::rng().fill(&mut randomness);
    let presentation = credential.present(server, &keys.secret, randomness);
    let user = hex(&keys.public_params());
    let password = Zeroizing::new(hex(&zkgroup::serialize(&presentation)));
    let token = Zeroizing::new(STANDARD.encode(format!("{user}:{}", password.as_str())));
    let mut value = HeaderValue::from_str(&format!("Basic {}", token.as_str()))
        .map_err(|_| Error::Internal("group credentials are not a header value"))?;
    value.set_sensitive(true);
    Ok(value)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
