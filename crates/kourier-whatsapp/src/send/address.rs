//! Where a send goes and how it names people and messages (W5a, chunk 3). PURE: no I/O, no clock, no
//! logging; the inputs (the account's LID migration, the store's `lid_pn_mapping`, our own ids, a group's
//! addressing mode) are read by the caller.
//!
//! THE CHAT. [`jid_of`] is the inverse of `map::conv_of`: `pn:<d>` -> `<d>@s.whatsapp.net`, `lid:<d>` ->
//! `<d>@lid`, `wag:<g>` -> `<g>@g.us`; every other key is refused, so the backend sends only to the chat the
//! core named (D6 in the brief §2.5 item (4): no code path sends to an id it did not get from `req.conv`).
//! A hosted id arrives as `pn:`/`lid:` (`map::scheme_of`) and goes back on the ordinary server.
//!
//! THE NAMESPACE (decision D20, critic M2). Upstream reconciles the namespace of the DM MESSAGE stanza only
//! (`resolve_dm_wire_jid`, `src/client/lid_pn.rs:819` at the pin, `pub(crate)`): an account that is 1:1
//! LID-migrated (`Device.lid_migrated` or the `lid_one_on_one_migration_enabled` prop, `is_lid_migrated`)
//! writes a 1:1 chat as its LID when the mapping is known, an unmigrated one as its number when the
//! mapping is known, and otherwise the id stays as it is. Receipts (`mark_as_read`), typing
//! (`chatstate().send`) and the keys inside a protocol message or a reaction go out AS WRITTEN, so they
//! take [`Addressing::chat`], which applies the same rule. In a group a person (a key's `participant`, a
//! receipt's `participant`, a mention) is written in the group's addressing mode.
//!
//! THE KEY ([`Addressing::key`]): whatsmeow's `BuildMessageKey` (`send.go:502`), which upstream's sender
//! revoke follows (`src/send/actions.rs` `RevokeType::Sender`): our own message is `fromMe = true` with no
//! `participant`; another's is `fromMe = false`, with the author as `participant` in a group. `fromMe` also
//! decides the wire attribute of a revoke (the probe: a REVOKE keyed `fromMe = false` is an admin revoke,
//! `edit="8"`). The receive map reads such a key from our other device back to the same target
//! (`map/modifier.rs` `Who::target`, the table there). ONE EXCEPTION (fix F10, WhatsApp Web's own keys, upstream at
//! the pin): in a GROUP the key of an EDIT or a REACTION to our own message names us as the `participant`, in the
//! group's addressing mode (`src/send/actions.rs` `message_ref_addon_key`: "add-ons carry the original group author";
//! `src/client/messaging.rs` `edit_message_inner`: `get_own_jid_for_group`), where receivers that index a group message by
//! (chat, participant, id) look for it; only the sender REVOKE keeps none ([`KeyFor`]). The OTHER author a key names
//! (a reaction to a member's message) is read by the whole group, so it follows the rule of a mention (fix F20,
//! [`Addressing::public_person`]): a number is never written into a group whose mode is `Lid` or unknown, and the
//! send fails instead ([`Unaddressable::NumberInLidGroup`] for good, [`Unaddressable::GroupModeUnknown`] to try again).

use kourier_model::{ConvKey, MsgTarget, TargetAuthor, UserId};
use wacore_binary::{Jid, Server};
use waproto::whatsapp as wa;

use crate::map::OwnIds;

/// Why a conversation, a person or a target has no address on the wire. Log-safe: a reason, never an id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unaddressable {
    /// A conversation kind WhatsApp has no chat for here (a list, a channel) or a scheme of another network.
    NotAChat,
    /// The id's value is not a WhatsApp user or group number.
    Malformed,
    /// The target names another conversation than the one the request is for.
    OtherConversation,
    /// A group target whose author is not known: a key without its `participant` names no message there.
    UnknownAuthor,
    /// One of our own ids is needed and neither is known yet.
    NoOwnId,
    /// A phone number would have to be written in a group that hides its members' numbers (a LID-addressed group,
    /// or one whose mode is unknown) and the mapping knows no LID for it (fix F7): every member would read it. A
    /// mention or a quote leaves the person out; a KEY cannot (fix F20), so there the send fails for good (a key
    /// reports an unknown mode as [`Self::GroupModeUnknown`]).
    NumberInLidGroup,
    /// A KEY would have to name a person by number in a group whose addressing mode is not known (the ask failed;
    /// fix F20): the mode may hide numbers, so nothing is written, and the mode is asked again at the next send.
    GroupModeUnknown,
}

/// What a key is for (fix F10): the keys of an edit and a reaction to our own message in a group name us, the
/// sender revoke's does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyFor {
    /// The original of an edit.
    Edit,
    /// The message a reaction is about.
    Reaction,
    /// The message a revoke takes back (upstream's sender revoke: no `participant`).
    Revoke,
}

/// The two namespaces a person's id is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespace {
    /// The phone number (`@s.whatsapp.net`).
    Pn,
    /// The LID (`@lid`).
    Lid,
}

/// The store's `lid_pn_mapping`, as the caller reads it (a phone may have several LIDs: the latest one is
/// meant, upstream's `get_current_lid`).
pub(crate) trait Mapping {
    /// The current LID user of a phone-number user.
    fn lid_of(&self, pn: &str) -> Option<String>;
    /// The phone-number user of a LID user.
    fn pn_of(&self, lid: &str) -> Option<String>;
}

/// The chat a conversation key stands for, the inverse of `map::conv_of`.
pub(crate) fn jid_of(conv: &ConvKey) -> Result<Jid, Unaddressable> {
    match conv {
        ConvKey::Direct(user) => user_jid(user),
        ConvKey::Group(id) => {
            let group = id.strip_prefix("wag:").ok_or(Unaddressable::NotAChat)?;
            // A group id is digits, the old form `<creator>-<time>` digits and one dash.
            let parts: Vec<&str> = group.split('-').collect();
            if parts.len() > 2 || !parts.iter().all(|part| is_number(part)) {
                return Err(Unaddressable::Malformed);
            }
            Ok(Jid::group(group))
        }
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => Err(Unaddressable::NotAChat),
    }
}

/// A person's id on its own server: `pn:` and `lid:` only.
fn user_jid(user: &UserId) -> Result<Jid, Unaddressable> {
    let server = match user.scheme() {
        "pn" => Server::Pn,
        "lid" => Server::Lid,
        _ => return Err(Unaddressable::NotAChat),
    };
    if !is_number(user.value()) {
        return Err(Unaddressable::Malformed);
    }
    Ok(Jid::new(user.value(), server))
}

fn is_number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// A person's bare id in `ns` when the mapping knows the other form, else as it is (upstream's
/// `swap_pn_lid_namespace` with its fallback). Ids of other servers are returned unchanged.
pub(crate) fn in_namespace(jid: &Jid, ns: Namespace, map: &dyn Mapping) -> Jid {
    let bare = jid.to_non_ad();
    match (bare.server, ns) {
        (Server::Pn, Namespace::Lid) => map
            .lid_of(&bare.user)
            .map_or(bare, |lid| Jid::new(lid, Server::Lid)),
        (Server::Lid, Namespace::Pn) => map
            .pn_of(&bare.user)
            .map_or(bare, |pn| Jid::new(pn, Server::Pn)),
        _ => bare,
    }
}

/// What a send needs to write ids the way the peer and our phone read them.
pub(crate) struct Addressing<'a> {
    /// The account is 1:1 LID-migrated (upstream's `Client::is_lid_migrated`).
    pub(crate) migrated: bool,
    /// The store's PN <-> LID pairs.
    pub(crate) map: &'a dyn Mapping,
    /// Our own ids.
    pub(crate) own: &'a OwnIds,
}

impl Addressing<'_> {
    /// The namespace of a 1:1 chat on the wire: the account's migration decides (upstream's rule).
    pub(crate) fn direct_namespace(&self) -> Namespace {
        if self.migrated {
            Namespace::Lid
        } else {
            Namespace::Pn
        }
    }

    /// The chat as receipts, typing and message keys name it: a 1:1 chat in the account's namespace (when
    /// the mapping is known), a group as it is (D20).
    pub(crate) fn chat(&self, conv: &ConvKey) -> Result<Jid, Unaddressable> {
        let jid = jid_of(conv)?;
        Ok(match conv {
            ConvKey::Direct(_) => in_namespace(&jid, self.direct_namespace(), self.map),
            _ => jid,
        })
    }

    /// A person's id in `ns`: one of ours from our own ids (the asked form first, else the other), anyone
    /// else's through the mapping.
    pub(crate) fn person(&self, user: &UserId, ns: Namespace) -> Result<Jid, Unaddressable> {
        if self.own.is_own(user) {
            return self.own_in(ns);
        }
        Ok(in_namespace(&user_jid(user)?, ns, self.map))
    }

    /// A person's id in `ns` when it is known (a 1:1 chat's, a group's addressing mode), else as the core
    /// holds it (ours: the LID first, a group's default addressing).
    pub(crate) fn person_in(
        &self,
        user: &UserId,
        ns: Option<Namespace>,
    ) -> Result<Jid, Unaddressable> {
        match ns {
            Some(ns) => self.person(user, ns),
            None if self.own.is_own(user) => self.own_in(Namespace::Lid),
            None => user_jid(user),
        }
    }

    /// A person's id as the MEMBERS of a group read it (fix F7, privacy LOW): the `@name` and `mentionedJid` of a
    /// mention, the `participant` of a quote. In a group whose mode is `Lid` or unknown NEVER a phone number: a person
    /// the core holds by number whose LID the mapping does not know is refused ([`Unaddressable::NumberInLidGroup`]),
    /// where [`Self::person_in`] would write the number for every member to read. Only a group addressed by
    /// numbers (`Some(Namespace::Pn)`) names numbers. Not for a 1:1 chat (two people) and not for what is addressed
    /// to the server alone (a receipt's `sender`).
    pub(crate) fn public_person(
        &self,
        user: &UserId,
        group: Option<Namespace>,
    ) -> Result<Jid, Unaddressable> {
        let jid = self.person_in(user, group)?;
        if jid.server == Server::Pn && group != Some(Namespace::Pn) {
            return Err(Unaddressable::NumberInLidGroup);
        }
        Ok(jid)
    }

    /// The author a KEY names, as every member of the group reads it (fix F20, re-check N2): the key's `participant`
    /// is read by the whole group like a mention, so [`Self::public_person`]'s rule holds: never a number in a group
    /// whose mode is `Lid`, nor in one whose mode is unknown. A key cannot leave its person out (without its
    /// `participant` it names no message), so the refusal fails the send: [`Unaddressable::NumberInLidGroup`] in a LID
    /// group (for good, until the mapping learns the LID), [`Unaddressable::GroupModeUnknown`] when the mode could
    /// not be read (the next send asks again). Only another's id gets here: our own message's key is built by the
    /// caller from [`Self::own_in`].
    fn key_author(&self, user: &UserId, group: Option<Namespace>) -> Result<Jid, Unaddressable> {
        match self.public_person(user, group) {
            Err(Unaddressable::NumberInLidGroup) if group.is_none() => {
                Err(Unaddressable::GroupModeUnknown)
            }
            found => found,
        }
    }

    /// Our own bare id in `ns`, else in the other namespace.
    pub(crate) fn own_in(&self, ns: Namespace) -> Result<Jid, Unaddressable> {
        let (first, second) = match ns {
            Namespace::Lid => (&self.own.lid, &self.own.pn),
            Namespace::Pn => (&self.own.pn, &self.own.lid),
        };
        user_jid(
            first
                .as_ref()
                .or(second.as_ref())
                .ok_or(Unaddressable::NoOwnId)?,
        )
    }

    /// The key of the message `target` names in the conversation `conv` (a reaction's, an edit's or a revoke's
    /// key, a quote's id and participant). `group` is the group's addressing mode when known: the author is
    /// written in it; unknown, the author is written in the form the core holds. In a group, our own message's key
    /// names us for an edit and a reaction, not for a revoke ([`KeyFor`], fix F10).
    pub(crate) fn key(
        &self,
        conv: &ConvKey,
        target: &MsgTarget,
        group: Option<Namespace>,
        used_for: KeyFor,
    ) -> Result<wa::MessageKey, Unaddressable> {
        self.key_as_received(conv, target, group, used_for, None)
    }

    /// [`Self::key`] for a message whose own key the backend still knows (fix F3, PROTOCOL-6): an admin's revoke must
    /// carry the original key's `participant` exactly as the message arrived ("Do NOT convert LID to PN - pass through
    /// unchanged like WhatsApp Web does", upstream `src/send/actions.rs:139-146`; whatsmeow writes `sender.ToNonAD()`
    /// as given). `received_as` is the author in the form the stanza named him in; when it is `Some`, another member's
    /// key takes it as it is, never re-derived into the group's CURRENT addressing mode (a group may have migrated to
    /// LIDs since, or the core may hold the other id of a merged person). Every member already read that form in the
    /// message's own key, so writing it again tells nobody anything new. When it is `None` the author is written in the
    /// current mode as before.
    pub(crate) fn key_as_received(
        &self,
        conv: &ConvKey,
        target: &MsgTarget,
        group: Option<Namespace>,
        used_for: KeyFor,
        received_as: Option<&UserId>,
    ) -> Result<wa::MessageKey, Unaddressable> {
        if target.conv.as_ref().is_some_and(|stated| stated != conv) {
            return Err(Unaddressable::OtherConversation);
        }
        let chat = self.chat(conv)?;
        let mut key = wa::MessageKey::default();
        key.remote_jid = Some(chat.to_string());
        key.id = Some(target.local.to_string());
        let author = match &target.author {
            TargetAuthor::Own => None,
            TargetAuthor::User(user) if self.own.is_own(user) => None,
            TargetAuthor::User(user) => Some(user),
            // In a group a key without its `participant` names no message; in a 1:1 chat the receive map never
            // leaves an author unknown, and without one `fromMe` cannot be told: refused in both.
            TargetAuthor::Unknown => return Err(Unaddressable::UnknownAuthor),
        };
        key.from_me = Some(author.is_none());
        if let ConvKey::Group(_) = conv {
            key.participant = match (author, used_for) {
                (Some(user), _) => Some(
                    match received_as {
                        Some(form) => user_jid(form)?,
                        None => self.key_author(user, group)?,
                    }
                    .to_string(),
                ),
                (None, KeyFor::Edit | KeyFor::Reaction) => {
                    Some(self.own_in(group.unwrap_or(Namespace::Lid))?.to_string())
                }
                (None, KeyFor::Revoke) => None,
            };
        }
        Ok(key)
    }
}

#[cfg(test)]
mod tests;
