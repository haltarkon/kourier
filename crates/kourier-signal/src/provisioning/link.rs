//! The linking flow (§6.5 steps 1-6; ports `SD/ts/textsecure/Provisioner.preload.ts` and
//! `AccountManager.preload.ts` `#doCreateAccount`): device name, provisioning socket and QR
//! code, the phone's `ProvisionMessage`, our keys and registration ids, `PUT /v1/devices/link`
//! on the unauthenticated socket, then everything persisted in one zone together with the sync
//! request jobs and `AccountReady`, the first durable event of the link. Steps 7-8 (auth connect,
//! one-time prekeys, the sync request jobs, `Done`) run on the connection once the account exists,
//! so a crash after step 6 resumes them.
//!
//! A login is never torn down in the middle of the link request: `CancelLogin`, `Logout` and
//! `Shutdown` ask it to stop ([`link`]'s `stop`), and it ends at the next point where it knows
//! what the server holds. Before the request nothing is registered ([`LinkEnd::Stopped`]); a
//! request already on its way is awaited, and a device it created is handed back unstored
//! ([`LinkEnd::Unwanted`]) so the actor can remove it from the account. Dropping the task
//! there would leave a linked device on the user's phone that nobody has the password of.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::Method;
use kourier_backend_api::{BackendEvent, Relink};
use kourier_model::{
    BackendError, ErrorKind, InputField, InputKind, LoginStep, LoginStepKind, Redacted,
};
use libsignal_core::{Aci, DeviceId};
use libsignal_protocol::Timestamp;
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    ProvisionedAccount, ProvisioningCipher, ProvisioningError, encrypt_device_name,
    normalize_device_name,
};
use crate::actor::Shared;
use crate::db::{self, Account, KeyIds};
use crate::error::{Error, Result};
use crate::keys::{SignedPreKeyJson, generate_kyber_prekey, generate_signed_prekey};
use crate::maintenance::{CAPABILITIES, Capabilities, enqueue_after_link};
use crate::net::{
    Connected, ProvisioningEvent, SignalTransport, json_request, parse_json, with_basic_auth,
};
use crate::outbox::append_events;
use crate::service_id::{ACI_SCHEME, parse_user_id};
use crate::store::{IdentityKind, insert_kyber_prekey, insert_signed_prekey, save_own_identity};

/// QR codes shown per login before giving up (`Provisioner.preload.ts:96-107`).
const MAX_ROTATIONS: u32 = 6;
/// How often a new QR code (provisioning socket) is shown (`ROTATION_INTERVAL`).
pub(crate) const ROTATION_INTERVAL: Duration = Duration::from_secs(45);
/// Provisioning sockets kept open at once (`MAX_OPEN_SOCKETS`): the code on screen and the one
/// before it.
const MAX_OPEN_SOCKETS: usize = 2;
/// Wait before the next attempt when a new provisioning socket could not be opened while an
/// older code is still open (with none open, the login fails at once).
const OPEN_RETRY: Duration = Duration::from_secs(2);
/// How long to wait for the provisioning address after the socket opened.
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Field id of the device-name input.
pub(crate) const DEVICE_NAME_FIELD: &str = "device_name";
/// Device name when neither the user nor the hostname gives one.
const FALLBACK_DEVICE_NAME: &str = "Kourier";

/// Input for a running login, forwarded from `Command::SubmitLoginInput`.
#[derive(Debug)]
pub(crate) struct LoginInput {
    /// The step answered.
    pub(crate) txn: u64,
    /// `(field id, value)`.
    pub(crate) values: Vec<(String, Redacted<String>)>,
}

/// Login step names (`LoginStep::step_id`).
mod step {
    pub(super) const DEVICE_NAME: &str = "device-name";
    pub(super) const QR: &str = "qr";
    pub(super) const LINKING: &str = "linking";
}

/// Why a login failed, ready for `LoginStepKind::Failed`.
pub(crate) type LoginError = BackendError;

/// How a login ended that did not leave a linked account in `backend.db`.
#[derive(Debug)]
pub(crate) enum LinkEnd {
    /// It failed; the user is told why.
    Failed(LoginError),
    /// It was asked to stop before the link request left: the server has registered nothing.
    Stopped,
    /// The server linked the device, but this login does not keep it: it was asked to stop
    /// while the link request was on its way (`error` is `None`), or the account could not be
    /// stored (`error` says why, for the user). Nothing of it is in `backend.db`; the actor
    /// removes the device from the account with these credentials
    /// (`DELETE /v1/devices/<id>`), or it would stay on the user's phone with nobody behind it.
    Unwanted {
        /// The device the server linked.
        device: Box<Account>,
        /// Why the login failed, when it was not stopped.
        error: Option<LoginError>,
    },
}

impl From<LoginError> for LinkEnd {
    fn from(error: LoginError) -> Self {
        Self::Failed(error)
    }
}

fn failed(kind: ErrorKind, detail: impl Into<String>, user: impl Into<String>) -> LoginError {
    BackendError::new(kind, detail).with_user_message(user)
}

fn from_error(e: &Error) -> LoginError {
    e.to_backend_error()
        .with_user_message("Linking failed; check the connection and try again.")
}

fn from_provisioning(e: &ProvisioningError) -> LoginError {
    match e {
        ProvisioningError::WrongAccount => failed(
            ErrorKind::InvalidInput,
            e.to_string(),
            "This is a different Signal account; add it as a new account.",
        ),
        ProvisioningError::NoPhoneNumber => {
            failed(ErrorKind::Unsupported, e.to_string(), e.to_string())
        }
        _ => failed(
            ErrorKind::InvalidInput,
            e.to_string(),
            "The phone sent data Kourier could not read; try linking again.",
        ),
    }
}

/// Whether the phone number `number` ends with the digits of `suffix` (the last digits the
/// user gave for the account to link). Only digits count on both sides; an expectation
/// without a digit matches nothing, so a mistyped one fails closed.
fn number_ends_with(number: &str, suffix: &str) -> bool {
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let (number, suffix) = (digits(number), digits(suffix));
    !suffix.is_empty() && number.ends_with(&suffix)
}

/// The machine's host name, the default device name (§6.5 step 1).
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| normalize_device_name(&h))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| FALLBACK_DEVICE_NAME.to_owned())
}

/// Runs steps 1-6 and returns the persisted account. Every step goes to the core as an
/// ephemeral `Login` event; the caller reports a failure as `LoginStepKind::Failed`.
///
/// `stop` asks the login to end (`CancelLogin`, `Logout`, `Shutdown`): it does so at once
/// while it waits for the user or the phone, and after the answer of a link request that is
/// already on its way (module docs).
pub(crate) async fn link<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    relink: Option<Relink>,
    mut inputs: mpsc::UnboundedReceiver<LoginInput>,
    stop: CancellationToken,
) -> std::result::Result<Account, LinkEnd> {
    let expect_own = relink.as_ref().map(expected_aci).transpose()?.flatten();
    // A fresh link asks the phone for the account's history (§14.2 item 14), when the
    // offer is switched on (`Tuning::transfer_offer`: off until the import exists); a
    // relink of a logged-out account that kept its chats never does: the archive would be
    // laid over a history that is there.
    let offer = relink.is_none() && shared.tuning.transfer_offer;

    // Step 1: the device name.
    let txn = 1;
    let default_name = hostname();
    emit(
        &shared,
        step::DEVICE_NAME,
        txn,
        LoginStepKind::Input {
            fields: vec![InputField {
                id: DEVICE_NAME_FIELD.to_owned(),
                label: "Device name".to_owned(),
                kind: InputKind::Text,
                default: Some(default_name.clone()),
                required: true,
            }],
        },
    )
    .await?;
    let device_name = loop {
        let input = tokio::select! {
            () = stop.cancelled() => return Err(LinkEnd::Stopped),
            input = inputs.recv() => input,
        };
        let Some(input) = input else {
            return Err(failed(
                ErrorKind::Internal,
                "login cancelled",
                "Linking was cancelled.",
            )
            .into());
        };
        if input.txn != txn {
            tracing::debug!(got = input.txn, txn, "stale login input ignored");
            continue;
        }
        let name = input
            .values
            .iter()
            .find(|(id, _)| id == DEVICE_NAME_FIELD)
            .map(|(_, value)| normalize_device_name(value.expose()))
            .filter(|name| !name.is_empty())
            .unwrap_or(default_name);
        break Zeroizing::new(name);
    };

    // Steps 2-3: QR codes until the phone answers.
    let Scanned {
        account: provisioned,
        answered,
        open,
    } = scan(&shared, txn, expect_own, offer, &stop).await?;
    // From the phone's provisioning message (`answered`) to the answered link request: the
    // Android phone waits 60 s for the new device to appear before it gives a history
    // transfer up (`LinkDeviceViewModel.kt` `waitForDeviceToBeLinked`). Nothing avoidable
    // stands between the two: the QR sockets that are still open (each close can take
    // seconds) are closed beside the link request, not before it.
    let closing = shared.clone();
    tokio::task::spawn_local(async move { close_all(&closing, open).await });
    // The account the user meant to link (`BackendConfig::expect_number_suffix`): looked at
    // before anything is registered, so the wrong phone scanning the code links nothing.
    if let Some(expected) = &shared.config.expect_number_suffix
        && !number_ends_with(&provisioned.number, expected.expose())
    {
        tracing::warn!(
            "the account that scanned the code is not the expected one; nothing was linked"
        );
        return Err(failed(
            ErrorKind::InvalidInput,
            "the scanned account is not the expected one",
            "This is not the account Kourier was told to link. Nothing was linked. \
             Scan the code with the other phone.",
        )
        .into());
    }
    emit(
        &shared,
        step::LINKING,
        txn,
        LoginStepKind::Progress {
            label: "Linking".to_owned(),
            fraction: None,
        },
    )
    .await?;

    // Steps 4-6.
    register(&shared, &provisioned, &device_name, offer, answered, &stop).await
}

/// The ACI a relink must find (§4.3 step 6). Fails closed: a relink that names own ids but no
/// (valid) ACI cannot be checked before `PUT /v1/devices/link`, so it is refused. An empty
/// `expect_own` (the core knows no ids) checks nothing, as the core does.
fn expected_aci(relink: &Relink) -> std::result::Result<Option<Aci>, LoginError> {
    let refused = |detail: &'static str| {
        failed(
            ErrorKind::InvalidInput,
            detail,
            "This account cannot be relinked.",
        )
    };
    for user in &relink.expect_own {
        if user.scheme() == ACI_SCHEME {
            return match parse_user_id(user) {
                Ok(crate::service_id::SignalUser::Aci(aci)) => Ok(Some(aci)),
                _ => Err(refused("relink names a malformed ACI")),
            };
        }
    }
    if relink.expect_own.is_empty() {
        Ok(None)
    } else {
        Err(refused("relink names no ACI"))
    }
}

async fn emit<T: SignalTransport>(
    shared: &Shared<T>,
    step_id: &str,
    txn: u64,
    kind: LoginStepKind,
) -> std::result::Result<(), LoginError> {
    shared
        .emit(BackendEvent::Login(LoginStep {
            step_id: step_id.to_owned(),
            txn,
            kind,
        }))
        .await
        .map_err(|e| from_error(&e))
}

/// One open provisioning socket and the cipher of the QR code it backs.
struct QrSocket<S> {
    socket: Arc<S>,
    events: mpsc::UnboundedReceiver<ProvisioningEvent>,
    cipher: ProvisioningCipher,
    /// Which rotation opened it (log-safe).
    rotation: u32,
}

/// The next event of any open socket, with that socket's index; `None` when its stream ended.
/// Pending forever while no socket is open.
async fn next_event<S>(sockets: &mut VecDeque<QrSocket<S>>) -> (usize, Option<ProvisioningEvent>) {
    std::future::poll_fn(|cx| {
        for (i, qr) in sockets.iter_mut().enumerate() {
            if let Poll::Ready(event) = qr.events.poll_recv(cx) {
                return Poll::Ready((i, event));
            }
        }
        Poll::Pending
    })
    .await
}

/// Steps 2-3 (`Provisioner.preload.ts` `#loop`): a new provisioning socket and QR code every
/// rotation, at most [`MAX_ROTATIONS`]. Up to [`MAX_OPEN_SOCKETS`] stay open, so the previous
/// code keeps working while the next one is shown and a phone that scanned it just before a
/// rotation still links. The phone's message is accepted on any open socket. When no socket is
/// open and a new one cannot be opened, the login fails with a connection error (Desktop).
///
/// Ends with [`LinkEnd::Stopped`] as soon as `stop` is cancelled, and the open sockets are
/// closed then, as after a failure. When the phone answered, the sockets that are still
/// open are handed back with its message: closing them takes time the phone is counting
/// (`link`), so the caller closes them beside the link request.
async fn scan<T: SignalTransport>(
    shared: &Shared<T>,
    txn: u64,
    expect_own: Option<Aci>,
    offer: bool,
    stop: &CancellationToken,
) -> std::result::Result<Scanned<T::Socket>, LinkEnd> {
    let mut open = VecDeque::new();
    let outcome = tokio::select! {
        () = stop.cancelled() => Err(LinkEnd::Stopped),
        outcome = scan_sockets(shared, txn, expect_own, offer, &mut open) => {
            outcome.map_err(LinkEnd::Failed)
        }
    };
    match outcome {
        Ok((account, answered)) => Ok(Scanned {
            account,
            answered,
            open,
        }),
        Err(end) => {
            close_all(shared, open).await;
            Err(end)
        }
    }
}

/// What a scan that the phone answered hands back.
struct Scanned<S> {
    /// The phone's provisioning message, decrypted.
    account: ProvisionedAccount,
    /// When that message was taken off its socket: the moment the phone starts to wait
    /// for the new device.
    answered: std::time::Instant,
    /// The QR sockets that are still open.
    open: VecDeque<QrSocket<S>>,
}

/// Closes the QR sockets of a scan, one after another.
async fn close_all<T: SignalTransport>(shared: &Shared<T>, open: VecDeque<QrSocket<T::Socket>>) {
    for qr in open {
        shared.net.disconnect(&qr.socket).await;
    }
}

async fn scan_sockets<T: SignalTransport>(
    shared: &Shared<T>,
    txn: u64,
    expect_own: Option<Aci>,
    offer: bool,
    open: &mut VecDeque<QrSocket<T::Socket>>,
) -> std::result::Result<(ProvisionedAccount, std::time::Instant), LoginError> {
    let rotation_every = shared.tuning.qr_rotation;
    for rotation in 0..MAX_ROTATIONS {
        // Open the next code while the previous one stays valid; its phone may answer meanwhile.
        let opened = {
            let opening = open_qr(shared, rotation, offer);
            tokio::pin!(opening);
            loop {
                tokio::select! {
                    opened = &mut opening => break opened,
                    (index, event) = next_event(open) => {
                        if let Some(account) = handle(open, index, event, expect_own)? {
                            return Ok(account);
                        }
                    }
                }
            }
        };
        let deadline = match opened {
            Some((qr, url)) => {
                open.push_back(qr);
                // The oldest code closes before the new one shows: from then on exactly the code
                // on screen and the one before it work.
                while open.len() > MAX_OPEN_SOCKETS {
                    if let Some(oldest) = open.pop_front() {
                        tracing::debug!(rotation = oldest.rotation, "closing the oldest QR socket");
                        shared.net.disconnect(&oldest.socket).await;
                    }
                }
                emit(
                    shared,
                    step::QR,
                    txn,
                    LoginStepKind::ShowQr {
                        data: Redacted::new(url),
                        valid_for: Some(rotation_every),
                    },
                )
                .await?;
                tokio::time::Instant::now() + rotation_every
            }
            // No code is open that the phone could scan: report the connection, as Desktop does
            // when its only socket fails (`Provisioner.preload.ts` `#loop`), instead of retrying
            // silently and ending with "expired" for codes that were never shown.
            None if open.is_empty() => {
                return Err(failed(
                    ErrorKind::Transient { retry_after: None },
                    "no provisioning socket could be opened",
                    "Kourier could not reach the Signal server. Check the connection and try \
                     again.",
                ));
            }
            // No new code this time: older ones stay valid; try again soon.
            None => tokio::time::Instant::now() + OPEN_RETRY,
        };
        // Wait for the phone on every open socket until the next rotation. When the newest
        // socket (the code on screen) drops, rotate at once.
        while let Ok((index, event)) = tokio::time::timeout_at(deadline, next_event(open)).await {
            let (was_newest, before) = (index + 1 == open.len(), open.len());
            if let Some(account) = handle(open, index, event, expect_own)? {
                return Ok(account);
            }
            if was_newest && open.len() < before {
                tracing::info!(rotation, "the QR code on screen dropped; rotating");
                break;
            }
        }
        tracing::info!(rotation, open = open.len(), "QR code rotation");
    }
    Err(failed(
        ErrorKind::Transient { retry_after: None },
        "no phone scanned the QR codes",
        "The QR code expired. Try again.",
    ))
}

/// Applies one event of socket `index`; removes the socket when it closed. A decrypt failure
/// (another account on a relink, a malformed message) ends the login. The phone's message
/// comes back with the moment it was taken up here, which is where the link's timing starts.
fn handle<S>(
    open: &mut VecDeque<QrSocket<S>>,
    index: usize,
    event: Option<ProvisioningEvent>,
    expect_own: Option<Aci>,
) -> std::result::Result<Option<(ProvisionedAccount, std::time::Instant)>, LoginError> {
    let Some(qr) = open.get(index) else {
        return Ok(None);
    };
    match event {
        Some(ProvisioningEvent::Envelope { bytes, ack }) => {
            let answered = std::time::Instant::now();
            if let Err(e) = ack.ack() {
                // The phone's message arrived; a lost ack only makes the server retry.
                tracing::debug!(error = %e, "acking the provisioning envelope failed");
            }
            tracing::info!(rotation = qr.rotation, "the phone answered a QR code");
            qr.cipher
                .decrypt(&bytes, expect_own)
                .map(|account| Some((account, answered)))
                .map_err(|e| from_provisioning(&e))
        }
        Some(ProvisioningEvent::Address { .. }) => {
            tracing::warn!(
                rotation = qr.rotation,
                "second provisioning address ignored"
            );
            Ok(None)
        }
        Some(ProvisioningEvent::ServerTimestamp(_)) => Ok(None),
        Some(ProvisioningEvent::Stopped(reason)) => {
            tracing::info!(rotation = qr.rotation, ?reason, "QR socket stopped");
            open.remove(index);
            Ok(None)
        }
        None => {
            open.remove(index);
            Ok(None)
        }
    }
}

/// Opens one provisioning socket and waits for its address; returns it with the QR code's URL.
/// `None` when the socket failed or sent no address in time (the caller retries).
async fn open_qr<T: SignalTransport>(
    shared: &Shared<T>,
    rotation: u32,
    offer: bool,
) -> Option<(QrSocket<T::Socket>, String)> {
    let Connected { socket, mut events } = match shared.net.connect_provisioning().await {
        Ok(connected) => connected,
        Err(e) => {
            tracing::warn!(rotation, error = %e, "provisioning socket failed");
            return None;
        }
    };
    let socket = Arc::new(socket);
    let address = loop {
        match tokio::time::timeout(ADDRESS_TIMEOUT, events.recv()).await {
            Ok(Some(ProvisioningEvent::Address { address, ack })) => {
                if let Err(e) = ack.ack() {
                    tracing::warn!(error = %e, "acking the provisioning address failed");
                    shared.net.disconnect(&socket).await;
                    return None;
                }
                break address;
            }
            Ok(Some(ProvisioningEvent::ServerTimestamp(_))) => {}
            Ok(Some(ProvisioningEvent::Envelope { .. })) => {
                tracing::warn!("provisioning envelope before the address; ignored");
            }
            Ok(Some(ProvisioningEvent::Stopped(reason))) => {
                tracing::info!(?reason, "provisioning socket stopped before the address");
                return None;
            }
            Ok(None) | Err(_) => {
                shared.net.disconnect(&socket).await;
                return None;
            }
        }
    };
    let cipher = ProvisioningCipher::generate(&mut rand::rng());
    let capabilities: &[&str] = if offer {
        &[super::CAPABILITY_HISTORY_TRANSFER]
    } else {
        &[]
    };
    let url = cipher.link_device_url(&address, capabilities);
    Some((
        QrSocket {
            socket,
            events,
            cipher,
            rotation,
        },
        url,
    ))
}

/// No `Debug`: it carries the encrypted device name (§9.1).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountAttributes {
    fetches_messages: bool,
    name: String,
    registration_id: u32,
    pni_registration_id: u32,
    capabilities: Capabilities,
}

/// Body of `PUT /v1/devices/link` (`WebAPI.preload.ts:2950-3005`). No `Debug`: it carries the
/// provisioning code, a credential (§9.1).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkRequest {
    verification_code: String,
    account_attributes: AccountAttributes,
    aci_signed_pre_key: SignedPreKeyJson,
    aci_pq_last_resort_pre_key: SignedPreKeyJson,
    pni_signed_pre_key: SignedPreKeyJson,
    pni_pq_last_resort_pre_key: SignedPreKeyJson,
}

/// Answer of `PUT /v1/devices/link`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkResponse {
    uuid: String,
    device_id: u32,
}

/// A fresh registration id in `1..=16383` (`SD/ts/Crypto.node.ts:44-46`).
fn registration_id() -> u32 {
    rand::rng().random_range(1..=16383)
}

/// Desktop's device password: 16 random bytes, base64, the `==` padding stripped.
fn password() -> Zeroizing<String> {
    let mut bytes = Zeroizing::new([0u8; 16]);
    rand::rng().fill(bytes.as_mut_slice());
    let mut encoded = Zeroizing::new(STANDARD.encode(bytes.as_slice()));
    encoded.truncate(22);
    encoded
}

/// Steps 4-6: keys and ids, the link request, then one zone that stores everything.
///
/// `stop` is looked at before the request leaves ([`LinkEnd::Stopped`]) and after its answer:
/// a device the server linked for a login nobody wants any more is returned unstored
/// ([`LinkEnd::Unwanted`]). The request itself is never abandoned, because only its answer
/// names the device that would have to be removed.
async fn register<T: SignalTransport>(
    shared: &Shared<T>,
    provisioned: &ProvisionedAccount,
    device_name: &str,
    offer: bool,
    answered: std::time::Instant,
    stop: &CancellationToken,
) -> std::result::Result<Account, LinkEnd> {
    let started = std::time::Instant::now();
    let rng = &mut rand::rng();
    let now_ms = shared.now_ms();
    let now = Timestamp::from_epoch_millis(now_ms);
    let password = password();
    let (aci_registration_id, pni_registration_id) = (registration_id(), registration_id());
    let (mut aci_ids, mut pni_ids) = (KeyIds::random(rng), KeyIds::random(rng));
    let local = |e: Error| from_error(&e);
    let aci_signed = generate_signed_prekey(&provisioned.aci_identity, aci_ids.signed, now, rng)
        .map_err(local)?;
    let aci_kyber =
        generate_kyber_prekey(&provisioned.aci_identity, aci_ids.kyber).map_err(local)?;
    let pni_signed = generate_signed_prekey(&provisioned.pni_identity, pni_ids.signed, now, rng)
        .map_err(local)?;
    let pni_kyber =
        generate_kyber_prekey(&provisioned.pni_identity, pni_ids.kyber).map_err(local)?;
    for ids in [&mut aci_ids, &mut pni_ids] {
        ids.signed = KeyIds::advance(ids.signed, 1);
        ids.kyber = KeyIds::advance(ids.kyber, 1);
    }
    let name = encrypt_device_name(rng, device_name, provisioned.aci_identity.public_key())
        .map_err(|_| {
            failed(
                ErrorKind::Internal,
                "device name encryption failed",
                "Linking failed.",
            )
        })?;
    let body = LinkRequest {
        verification_code: provisioned.provisioning_code.as_str().to_owned(),
        account_attributes: AccountAttributes {
            fetches_messages: true,
            name,
            registration_id: aci_registration_id,
            pni_registration_id,
            capabilities: CAPABILITIES,
        },
        aci_signed_pre_key: SignedPreKeyJson::signed(&aci_signed).map_err(local)?,
        aci_pq_last_resort_pre_key: SignedPreKeyJson::kyber(&aci_kyber).map_err(local)?,
        pni_signed_pre_key: SignedPreKeyJson::signed(&pni_signed).map_err(local)?,
        pni_pq_last_resort_pre_key: SignedPreKeyJson::kyber(&pni_kyber).map_err(local)?,
    };
    let aci_string = Uuid::from(provisioned.aci).hyphenated().to_string();
    let request = json_request(Method::PUT, "/v1/devices/link", &body)
        .and_then(|r| with_basic_auth(r, &aci_string, &password))
        .map_err(|e| from_error(&e.into()))?;

    let keys_made = std::time::Instant::now();
    let connected = tokio::select! {
        () = stop.cancelled() => return Err(LinkEnd::Stopped),
        connected = shared.net.connect_unauth() => connected,
    };
    let connected_at = std::time::Instant::now();
    let Connected { socket, events: _ } = connected.map_err(|e| from_error(&e.into()))?;
    let socket = Arc::new(socket);
    if stop.is_cancelled() {
        shared.net.disconnect(&socket).await;
        return Err(LinkEnd::Stopped);
    }
    // From here to the answer the login cannot be stopped: the server may link the device
    // at any moment, and only the answer names it.
    let response = shared.net.request(&socket, request).await;
    let requested = std::time::Instant::now();
    shared.net.disconnect(&socket).await;
    let response = response.map_err(|e| from_error(&e.into()))?;
    match response.status.as_u16() {
        200..=299 => {}
        409 => {
            return Err(failed(
                ErrorKind::Unsupported,
                "link refused with 409",
                "Kourier is too old for this account.",
            )
            .into());
        }
        411 => {
            return Err(failed(
                ErrorKind::InvalidInput,
                "link refused with 411",
                "This account has too many linked devices.",
            )
            .into());
        }
        status => {
            return Err(from_error(&Error::Status {
                what: "device link",
                status,
            })
            .into());
        }
    }
    let linked: LinkResponse = parse_json(&response).map_err(|e| from_error(&e.into()))?;
    if Uuid::try_parse(&linked.uuid).ok() != Some(Uuid::from(provisioned.aci)) {
        return Err(failed(
            ErrorKind::Internal,
            "the server linked another account",
            "Linking failed.",
        )
        .into());
    }
    let device_id = DeviceId::try_from(linked.device_id).map_err(|_| {
        failed(
            ErrorKind::Internal,
            "invalid device id from the server",
            "Linking failed.",
        )
    })?;

    let account = Account {
        aci: provisioned.aci,
        pni: provisioned.pni,
        e164: provisioned.number.clone(),
        device_id,
        password,
        aci_registration_id,
        pni_registration_id,
        profile_key: provisioned.profile_key.clone(),
        aci_identity: provisioned.aci_identity,
        pni_identity: provisioned.pni_identity,
        post_link_done: false,
    };
    if stop.is_cancelled() {
        // The device exists on the server and nobody wants it: not stored, handed back for
        // its removal.
        tracing::info!("the login was stopped while its link request was answered");
        return Err(LinkEnd::Unwanted {
            device: Box::new(account),
            error: None,
        });
    }
    let now_i = i64::try_from(now_ms).unwrap_or(i64::MAX);
    // The phone's key for the history transfer, when it sent one for a link that offered
    // `backup5`. A relink, and a fresh link while the offer is switched off, offered
    // nothing: a key that comes all the same is not kept and no transfer starts.
    let transfer_key = provisioned
        .ephemeral_backup_key
        .as_deref()
        .filter(|_| offer);
    if provisioned.ephemeral_backup_key.is_some() && !offer {
        tracing::warn!("the phone sent a history transfer key that was not asked for; ignored");
    }
    let stored = shared.db.zone(|tx| -> Result<()> {
        tx.execute(
            "INSERT INTO account(id, aci, pni, e164, device_id, password,
                   aci_registration_id, pni_registration_id, profile_key, master_key,
                   account_entropy_pool, storage_key, media_root_backup_key, read_receipts,
                   linked_at, env)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            kourier_db::rusqlite::params![
                aci_string,
                Uuid::from(provisioned.pni).hyphenated().to_string(),
                provisioned.number,
                u32::from(device_id),
                account.password.as_str(),
                aci_registration_id,
                pni_registration_id,
                provisioned.profile_key.as_slice(),
                provisioned.master_key.as_slice(),
                provisioned
                    .account_entropy_pool
                    .as_deref()
                    .map(|p| p.as_str()),
                provisioned.storage_service_key().as_slice(),
                provisioned
                    .media_root_backup_key
                    .as_deref()
                    .map(|k| k.as_slice()),
                provisioned.read_receipts,
                now_i,
                // The server this device now belongs to; every later start is held to it.
                crate::net::env::binding(&shared.config.env),
            ],
        )?;
        for (kind, identity, ids, signed, kyber) in [
            (
                IdentityKind::Aci,
                &account.aci_identity,
                aci_ids,
                &aci_signed,
                &aci_kyber,
            ),
            (
                IdentityKind::Pni,
                &account.pni_identity,
                pni_ids,
                &pni_signed,
                &pni_kyber,
            ),
        ] {
            db::insert_own_identity(tx, kind, identity, ids)?;
            save_own_identity(
                tx,
                &account.service_id(kind).service_id_string(),
                identity.identity_key(),
                now_i,
            )?;
            // The link request carried both keys and the server accepted it: confirmed.
            insert_signed_prekey(tx, kind, signed, true)?;
            insert_kyber_prekey(tx, kind, kyber, true, true, now_i)?;
        }
        // The history transfer starts in the same transaction as the account: a crash right
        // behind the link finds the key and the state, or no account at all.
        if let Some(key) = transfer_key {
            crate::transfer::begin(tx, key, now_i, shared.tuning.transfer_wait)?;
        }
        // The link's sync requests are durable jobs from here on (§6.5 step 8).
        enqueue_after_link(tx, now_i)?;
        // `AccountReady` is the first durable event of a link: the core marks the account
        // `Active` from it, before anything else this account receives (§4.3 step 2).
        append_events(
            tx,
            &[BackendEvent::AccountReady {
                own_ids: account.own_ids()?,
                device: Some(u32::from(device_id)),
                display_name: None,
                number: account.number(),
            }],
            now_i,
            &shared.wake,
        )?;
        Ok(())
    });
    if let Err(e) = stored {
        // Linked on the server and not kept here: the device must not stay on the account.
        tracing::error!(error = %e, "storing the linked account failed");
        return Err(LinkEnd::Unwanted {
            device: Box::new(account),
            error: Some(from_error(&e)),
        });
    }
    if transfer_key.is_some() {
        // Before anything connects: no envelope is taken until the transfer is over.
        shared.transfer.hold();
    }
    let stored_at = std::time::Instant::now();
    let micros = |d: std::time::Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
    // Numbers only (`crate::PERF_TARGET`): where the time between the phone's provisioning
    // message and the linked device went.
    tracing::trace!(
        target: crate::PERF_TARGET,
        name = crate::PERF_LINK_REGISTERED,
        // From the phone's message taken off its socket to the start of the link's own
        // work: its decryption, the check of the account and the "Linking" step.
        taken_us = micros(started - answered),
        keys_us = micros(keys_made - started),
        connect_us = micros(connected_at - keys_made),
        request_us = micros(requested - connected_at),
        store_us = micros(stored_at - requested),
        total_us = micros(stored_at - answered),
        linked_us = micros(requested - answered),
    );
    tracing::info!(
        account = ?account,
        history_transfer = transfer_key.is_some(),
        envelope_to_linked_ms = (requested - answered).as_millis() as u64,
        "device linked"
    );
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expected_account_is_told_by_the_last_digits_of_its_number() {
        assert!(number_ends_with("+15550101234", "1234"));
        assert!(number_ends_with("+15550101234", "01234"));
        // Only digits count, on both sides.
        assert!(number_ends_with("+1 555 010-1234", " 12-34 "));
        assert!(number_ends_with("+15550101234", "+15550101234"));
        assert!(!number_ends_with("+15550101234", "1235"));
        assert!(!number_ends_with("+15550101234", "234 1"));
        // Longer than the number, or no digit at all: nothing matches (fails closed).
        assert!(!number_ends_with("+1234", "91234"));
        assert!(!number_ends_with("+15550101234", ""));
        assert!(!number_ends_with("+15550101234", "abcd"));
    }

    #[test]
    fn password_and_registration_ids_have_desktop_shapes() {
        let p = password();
        assert_eq!(p.len(), 22);
        assert!(!p.contains('='));
        assert_ne!(*p, *password());
        for _ in 0..100 {
            assert!((1..=16383).contains(&registration_id()));
        }
    }

    #[test]
    fn relinks_check_the_aci_and_fail_closed_without_one() {
        use kourier_model::{AccountId, UserId};
        let relink = |ids: &[&str]| Relink {
            account: AccountId(1),
            expect_own: ids.iter().map(|id| UserId::parse(id).unwrap()).collect(),
        };
        let aci = "9d0652a3-dcc3-4d11-975f-74d61598733f";
        let pni = "pni:5f2e1b6c-7a1d-4c3e-9b8a-2d4f6e8a0c1b";
        assert_eq!(
            expected_aci(&relink(&[pni, &format!("aci:{aci}")])).unwrap(),
            Some(Aci::from(Uuid::parse_str(aci).unwrap())),
            "the ACI is found anywhere in the list"
        );
        assert_eq!(expected_aci(&relink(&[])).unwrap(), None, "nothing known");
        let refused = expected_aci(&relink(&[pni])).unwrap_err();
        assert_eq!(
            refused.user_message.as_deref(),
            Some("This account cannot be relinked.")
        );
    }

    #[test]
    fn link_request_serializes_like_desktop() {
        let rng = &mut rand::rng();
        let identity = libsignal_protocol::IdentityKeyPair::generate(rng);
        let now = Timestamp::from_epoch_millis(1);
        let s = generate_signed_prekey(&identity, 1, now, rng).unwrap();
        let k = generate_kyber_prekey(&identity, 2).unwrap();
        let body = LinkRequest {
            verification_code: "code".into(),
            account_attributes: AccountAttributes {
                fetches_messages: true,
                name: "bmFtZQ==".into(),
                registration_id: 1,
                pni_registration_id: 2,
                capabilities: CAPABILITIES,
            },
            aci_signed_pre_key: SignedPreKeyJson::signed(&s).unwrap(),
            aci_pq_last_resort_pre_key: SignedPreKeyJson::kyber(&k).unwrap(),
            pni_signed_pre_key: SignedPreKeyJson::signed(&s).unwrap(),
            pni_pq_last_resort_pre_key: SignedPreKeyJson::kyber(&k).unwrap(),
        };
        let json: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(json["verificationCode"], "code");
        assert_eq!(json["accountAttributes"]["fetchesMessages"], true);
        assert_eq!(json["accountAttributes"]["pniRegistrationId"], 2);
        assert_eq!(json["accountAttributes"]["capabilities"]["spqr"], true);
        assert_eq!(
            json["accountAttributes"]["capabilities"]["usernameChangeSyncMessage"],
            true
        );
        assert_eq!(json["aciPqLastResortPreKey"]["keyId"], 2);
        assert!(json["pniSignedPreKey"]["signature"].is_string());
    }
}
