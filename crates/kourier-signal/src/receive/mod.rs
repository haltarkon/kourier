//! The receive pipeline (§5.6, §6.6; ports `SD/ts/textsecure/MessageReceiver.preload.ts`).
//!
//! - [`stage1`]: one zone per batch decodes, applies the PNI rules, unseals and decrypts each
//!   envelope and stores the plaintext in `inbox` with its lane; facts learned while decrypting
//!   (identity replaced, PNI signature verified, the sender's number from a sealed-sender
//!   certificate) go into `core_outbox` in the same zone. The server acks go out only after
//!   COMMIT.
//! - [`stage2`]: one lane per conversation, lanes run concurrently and keep arrival order inside;
//!   a lane step turns ≤ 50 rows into core events ([`dispatch`]) in the zone that deletes them, so
//!   the events are in `core_outbox` exactly when the rows are gone. Content failures count
//!   towards the poison limit; network failures park the lane and never count.
//! - [`dispatch`]: the content mapping of §6.13 (`Content` → core events and backend effects).
//! - [`calls`]: call messages, group rings, group-call updates and the phone's call-event syncs
//!   as call lines. Kourier shows calls and never sends anything for one.

pub(crate) mod calls;
pub(crate) mod dispatch;
mod group_dispatch;
pub(crate) mod media;
pub(crate) mod stage1;
pub(crate) mod stage2;

use std::fmt::Write as _;

use sha2::{Digest as _, Sha256};

/// The failpoint right after a stage-1 commit, before any ack (§10.4).
pub(crate) const FAILPOINT_AFTER_INBOX_COMMIT: &str = "after_inbox_commit";
/// The failpoint before each server ack (§10.4).
pub(crate) const FAILPOINT_BEFORE_ACK: &str = "before_ack";
/// The failpoint after a stage-2 (or job) zone committed, its events still in `core_outbox`.
pub(crate) const FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT: &str = "after_backend_effects_commit";

/// The lane of rows without a conversation (receipts, read syncs, settings).
pub(crate) const MISC_LANE: &str = "misc";

/// `inbox.kind` of a decrypted `Content`.
pub(crate) const KIND_CONTENT: &str = "content";
/// `inbox.kind` of a server delivery receipt (no content).
pub(crate) const KIND_DELIVERY_RECEIPT: &str = "delivery_receipt";

/// The lane of a group's rows: `group:` and 16 bytes of SHA-256 over the group master key in
/// hex, so the key itself is not copied into another column (`crate::groups::lane` fetches the
/// group for them).
pub(crate) fn group_lane(master_key: &[u8]) -> String {
    let digest = Sha256::digest(master_key);
    let mut lane = String::with_capacity(6 + 32);
    lane.push_str("group:");
    for byte in &digest[..16] {
        let _ = write!(lane, "{byte:02x}");
    }
    lane
}

#[cfg(test)]
pub(crate) mod tests;
