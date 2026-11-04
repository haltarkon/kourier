//! The pre-pair refusal (W2c; decision D1 of `t1-decisions.md`): a pairing is judged BEFORE the device
//! exists, so Kourier never signs a pairing the login's rules refuse and stores nothing of the other
//! account.
//!
//! WHY. Upstream completes a pairing the moment its `<pair-success>` verifies: it writes the other
//! account's number, LID and signed identity into the store, sends `<pair-device-sign>` (the frame that
//! makes the phone list the companion) and reconnects as that device. The login's identity checks
//! ([`crate::link::Rules::decide`]) used to run on the event that follows all of that (W2a: the `Gate`),
//! so a refused scan had already made Kourier sign the pairing and put the other account's data into this profile.
//! The `<pair-success>` stanza carries BOTH ids (`<device jid=<number>:<device>@s.whatsapp.net
//! lid=<lid>:<device>@lid>`), so the SAME function can decide on the stanza, before upstream reads it.
//!
//! HOW. [`PrePair`] is the pairing half of the one stanza interceptor of a client
//! (`session::Stanzas`): upstream offers every `<iq>` that is no ping and matched no waiter to the
//! interceptors BEFORE its built-in IQ handler (`node_io.rs` `dispatch_node_scoped`), and a claimed `<iq>`
//! gets NO answer from upstream. A pairing that fails the rules is claimed here, answered with upstream's
//! own pair error (`<iq type=error><error code=500 text=internal-error/></iq>`, the frame whatsmeow's
//! `PrePairCallback` refusal sends too) and then told to the login machine, which ends the login with the
//! refusal's own `Failed` step: no store write, no `<pair-device-sign>`, no 515, nothing to log out.
//! "Kourier never signs the pairing and stores nothing of the other account" is what holds afterwards;
//! what the PHONE shows after the 500 is in no source (R1 records it if a wrong scan ever happens).
//!
//! THE CLAIM COPIES UPSTREAM'S GATE, not "`type=set`": `from` is the server (compared the way upstream
//! compares it, so a string and a JID-typed value both match), the request is an `<iq>` of any type, and
//! SOME child is tagged `pair-success`. EVERY such child is judged, and more than one is refused
//! (whatsmeow requires exactly one): upstream completes the FIRST handled child, and a later one that
//! the rules accept must not hide an earlier stranger's. The claim refuses BEFORE the HMAC and the
//! account signature are checked (decision L-b): refusing is always safe, and an accepted pairing is
//! still verified by upstream.
//!
//! THE SECOND LINE stays: [`crate::tap::Tap::deliver`] decides on `WaSignal::PairSuccess` as W2a built it.
//! It matters only if the claim did not see the pairing (a pin bump moved it). The pin-bump checklist
//! (`.claude/rules/whatsapp.md`): re-read upstream's `pair.rs` `handle_iq_scoped` gate, `handle_pair_success`'s `<device>`
//! parse and `node_io.rs`'s "interceptors before the IQ handler" order; `tests/prepair.rs` T-W1 (a wrong
//! scan gets no `pair-device-sign`) AND T-W3 (a right scan still gets it) are the tripwires, and must stay
//! in the same tier for ever: T-W1 alone also passes when the test phone's signature is wrong.
//!
//! WHAT IS LOGGED: one WARN line per refusal, a fixed token and whether the answer left, nothing else
//! (never the id, a JID, digits or the tail). Upstream's lines on this path are DEBUG and dropped by
//! [`crate::logs::keeps`].

use std::sync::Arc;

use kourier_model::Redacted;
use wacore_binary::{Node, NodeRef, OwnedNodeRef, SERVER_JID};
use whatsapp_rust::pair::PairUtils;
use whatsapp_rust::{Interception, StanzaInterceptor};

use crate::link::{Tell, Verdict};
use crate::tap::Tap;
use crate::translate::Paired;

/// The code and text of the pair error (upstream's own for a pair-success it cannot use).
pub(crate) const ANSWER_CODE: u16 = 500;
/// See [`ANSWER_CODE`].
pub(crate) const ANSWER_TEXT: &str = "internal-error";

/// A pairing stanza as the claim read it: the request id and the ids of every `pair-success` child.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Claim {
    /// The `<iq>`'s `id`, which an answer echoes. `None`: upstream would send nothing either.
    pub id: Option<Redacted<String>>,
    /// One entry per `pair-success` child, in order: its ids, or `None` when it names no device.
    pub pairs: Vec<Option<Paired>>,
}

impl std::fmt::Debug for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claim")
            .field("id", &self.id.is_some())
            .field("children", &self.pairs.len())
            .finish()
    }
}

/// Reads `node` as a pairing stanza: `None` unless it is an `<iq>` from the server with a
/// `pair-success` child (upstream's own gate, `pair.rs` `handle_iq_scoped`).
pub(crate) fn read(node: &NodeRef<'_>) -> Option<Claim> {
    if node.tag != "iq" {
        return None;
    }
    // Upstream's comparison, character for character: a `ValueRef` is equal to a string when it is
    // that string or a JID that prints as it.
    if node.get_attr("from").is_none_or(|v| v != SERVER_JID) {
        return None;
    }
    let pairs: Vec<Option<Paired>> = node
        .children()?
        .iter()
        .filter(|child| child.tag == "pair-success")
        .map(paired_of)
        .collect();
    if pairs.is_empty() {
        return None;
    }
    let id = node
        .get_attr("id")
        .map(|v| Redacted::new(v.as_str().into_owned()));
    Some(Claim { id, pairs })
}

/// The ids of one `pair-success` child, read with the calls upstream makes (`pair.rs`
/// `handle_pair_success`): a missing `jid` or `lid` is the empty user, which the rules refuse as
/// unreadable. `None` when the child has no `<device>` at all.
fn paired_of(success: &NodeRef<'_>) -> Option<Paired> {
    let device = success.get_optional_child_by_tag(&["device"])?;
    let mut parser = device.attrs();
    let jid = parser.optional_jid("jid").unwrap_or_default();
    let lid = parser.optional_jid("lid").unwrap_or_default();
    Some(Paired {
        pn: Redacted::new(jid.user.to_string()),
        lid: Redacted::new(lid.user.to_string()),
        device: jid.device,
    })
}

/// The frame that refuses the pairing request `id`: upstream's own builder and marshal, and the only
/// place of the crate that names it (the source check in `tests/builder.rs`). Root `iq`, attributes
/// `to="s.whatsapp.net"`, `type="error"`, `id=<the server's id>`, one child `error` with
/// `code="500"` and `text="internal-error"`; nothing of either account.
pub(crate) fn answer(id: &str) -> Node {
    PairUtils::build_pair_error_node(id, ANSWER_CODE, ANSWER_TEXT)
}

/// How a refusal reaches the wire: the error iq to send (none when the stanza had no id: upstream would
/// send nothing either) and what to do after, told whether the iq went. Built by the client from its
/// runtime and its weak handle on the client (`session::client`): the work runs as a TASK on the
/// client's runtime, never on the read loop that calls the interceptor. A client that is not there
/// (not started, stopped) calls `done(false)` at once.
pub(crate) type Dispatch = Arc<dyn Fn(Option<Node>, Box<dyn FnOnce(bool) + Send>) + Send + Sync>;

/// The pairing half of the client's one stanza interceptor (`session::Stanzas`).
pub(crate) struct PrePair {
    tap: Tap,
    dispatch: Dispatch,
}

impl PrePair {
    pub(crate) fn new(tap: Tap, dispatch: Dispatch) -> Self {
        Self { tap, dispatch }
    }
}

impl StanzaInterceptor for PrePair {
    /// Sync, never blocks, never panics (it runs on a dispatch task of the client). A stanza that is no
    /// pairing, or a pairing the rules let through, is `Pass`: upstream pairs, and the Forwarder's gate
    /// decides again on its event. A refused pairing is claimed: the error iq goes out FIRST (the
    /// dispatcher awaits the socket), THEN the login is told, so the client is not stopped under its own
    /// answer.
    fn intercept(&self, node: &OwnedNodeRef) -> Interception {
        let Some(claim) = read(node.get()) else {
            return Interception::Pass;
        };
        let Verdict::Refuse(refused) = self.tap.judge_pairing(&claim.pairs) else {
            return Interception::Pass;
        };
        let reply = claim.id.as_ref().map(|id| answer(id.expose()));
        let tap = self.tap.clone();
        (self.dispatch)(
            reply,
            Box::new(move |answered| {
                tracing::warn!(
                    refusal = refused.token,
                    answered,
                    "a pairing was refused before the device was made"
                );
                if let Tell::Login(paired) = refused.tell {
                    tap.deliver_claimed(paired);
                }
            }),
        );
        Interception::Handled
    }
}

#[cfg(test)]
mod tests;
