//! Roaming state (§4.1 principle 4, §4.9 row 4): the account row, contacts, conversations,
//! pinned order, removed conversations. Field-wise patches; `members.full` replaces, else delta.

use std::path::PathBuf;
use std::time::Duration;

use kourier_db::rusqlite::{OptionalExtension, params};
use kourier_model::{
    AccountCaps, AccountSettings, AccountSettingsPatch, BlobKey, BlobRef, Contact, ContactId,
    ContactPatch, ConvKey, Conversation, ConversationFeatures, ConversationId, ConversationPatch,
    Member, Patch, TrustLevel, UserId,
};

use super::Apply;
use crate::error::{CoreError, Result};
use crate::sql;

impl Apply<'_> {
    /// `AccountReady`: own ids, device, display name and number (the last two only when the
    /// backend states them); the account becomes `Active`, unless it is being removed.
    pub(crate) fn account_ready(
        &mut self,
        own_ids: Vec<UserId>,
        device: Option<u32>,
        display_name: Option<String>,
        number: Option<String>,
    ) -> Result<()> {
        // An account that is being removed stays so (§4.3 step 7): a late `AccountReady` must
        // not bring it back as `Active` when its removal then has to be finished at the next
        // start.
        // (An unlink in progress likewise stays: the actor started just for its logout
        // announces the account again.)
        let removing = matches!(
            self.acc.state,
            kourier_model::AccountState::Removing | kourier_model::AccountState::Unlinking
        );
        // One identity is one account of a profile (L1, `buttons.md` F2): a link (fresh or
        // again) that brings the primary own id of another account of the same network is
        // refused here, in the transaction that would make it an account, so two links that
        // end in the same moment cannot both pass. An account that is already Active keeps
        // running (a profile made before this rule may hold two).
        if !removing
            && self.acc.state != kourier_model::AccountState::Active
            && let Some(logged_out) = same_identity(self.conn, self.acc.id, own_ids.first())?
        {
            return Err(CoreError::AlreadyAdded { logged_out });
        }
        self.conn
            .prepare_cached(
                "UPDATE accounts SET own_ids = ?2, own_device = ?3,
                   display_name = coalesce(?4, display_name),
                   state = CASE WHEN ?5 THEN state ELSE 'Active' END,
                   state_reason = CASE WHEN ?5 THEN state_reason ELSE NULL END,
                   number = coalesce(?6, number),
                   created_at = CASE WHEN ?5 OR state = 'Active' THEN created_at ELSE ?7 END
                 WHERE id = ?1",
            )?
            .execute(params![
                self.acc.id.0,
                sql::to_json(&own_ids)?,
                device,
                display_name,
                removing,
                number,
                self.now
            ])?;
        // Direct chats with ourselves are notes to self.
        for id in &own_ids {
            self.conn
                .prepare_cached(
                    "UPDATE conversations SET is_self = 1 WHERE account_id = ?1 AND conv_key = ?2",
                )?
                .execute((self.acc.id.0, ConvKey::Direct(id.clone()).encode()))?;
        }
        self.acc.own_ids = own_ids;
        if !removing {
            self.acc.state = kourier_model::AccountState::Active;
        }
        self.ch.accounts = true;
        self.ch.account_ready = true;
        Ok(())
    }

    /// Network-synced account settings.
    pub(crate) fn account_settings(&mut self, patch: AccountSettingsPatch) -> Result<()> {
        let current: String = self
            .conn
            .prepare_cached("SELECT settings FROM accounts WHERE id = ?1")?
            .query_row([self.acc.id.0], |r| r.get(0))?;
        let mut settings: AccountSettings = sql::from_json(&current)?;
        if patch.apply(&mut settings) {
            self.conn
                .prepare_cached("UPDATE accounts SET settings = ?2 WHERE id = ?1")?
                .execute((self.acc.id.0, sql::to_json(&settings)?))?;
            self.acc.read_receipts = settings.read_receipts;
            self.ch.accounts = true;
        }
        Ok(())
    }

    /// Server limits and flags.
    pub(crate) fn account_caps(&mut self, caps: &AccountCaps) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE accounts SET caps = ?2 WHERE id = ?1")?
            .execute((self.acc.id.0, sql::to_json(caps)?))?;
        self.ch.accounts = true;
        Ok(())
    }

    // ---------- files (avatars) ----------

    fn load_file(&self, id: Option<i64>) -> Result<Option<BlobRef>> {
        let Some(id) = id else { return Ok(None) };
        let row: Option<(String, Vec<u8>)> = self
            .conn
            .prepare_cached("SELECT path, key FROM files WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(row.and_then(|(path, key)| {
            let key: [u8; 32] = key.try_into().ok()?;
            Some(BlobRef {
                path: PathBuf::from(path),
                key: BlobKey::new(key),
            })
        }))
    }

    /// Stores `blob` as the avatar files row `existing` (replaced in place) or a new row.
    fn store_avatar(&self, existing: Option<i64>, blob: Option<&BlobRef>) -> Result<Option<i64>> {
        match (existing, blob) {
            (Some(id), Some(b)) => {
                self.conn
                    .prepare_cached("UPDATE files SET path = ?2, key = ?3 WHERE id = ?1")?
                    .execute((
                        id,
                        b.path.to_string_lossy().into_owned(),
                        b.key.expose().as_slice(),
                    ))?;
                Ok(Some(id))
            }
            (None, Some(b)) => {
                self.conn
                    .prepare_cached(
                        "INSERT INTO files(account_id, path, key, purpose) VALUES (?1, ?2, ?3, 'avatar')",
                    )?
                    .execute((
                        self.acc.id.0,
                        b.path.to_string_lossy().into_owned(),
                        b.key.expose().as_slice(),
                    ))?;
                Ok(Some(self.conn.last_insert_rowid()))
            }
            (Some(id), None) => {
                self.conn
                    .prepare_cached("DELETE FROM files WHERE id = ?1")?
                    .execute([id])?;
                Ok(None)
            }
            (None, None) => Ok(None),
        }
    }

    // ---------- contacts ----------

    /// Loads a contact (`None` when unknown) and its avatar files row.
    pub(crate) fn load_contact(&self, user: &UserId) -> Result<Option<(Contact, Option<i64>)>> {
        type Row = (
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
            String,
            Option<Vec<u8>>,
            bool,
            bool,
            Option<i64>,
            String,
            Option<Vec<u8>>,
        );
        let row: Option<Row> = self
            .conn
            .prepare_cached(
                "SELECT id, profile_name, system_name, nickname, avatar_file, trust, fingerprint,
                   blocked, hidden, unregistered_at, identifiers, meta
                 FROM contacts WHERE account_id = ?1 AND user_id = ?2",
            )?
            .query_row((self.acc.id.0, user.as_str()), |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                ))
            })
            .optional()?;
        let Some(r) = row else { return Ok(None) };
        let contact = Contact {
            id: ContactId(r.0),
            account: self.acc.id,
            user: user.clone(),
            profile_name: r.1,
            system_name: r.2,
            nickname: r.3,
            avatar: self.load_file(r.4)?,
            trust: sql::from_enum_name(&r.5)?,
            identity_fingerprint: r.6,
            blocked: r.7,
            hidden: r.8,
            unregistered_at: r.9,
            identifiers: sql::from_json(&r.10)?,
            meta: sql::blob_from_sql(r.11),
        };
        Ok(Some((contact, r.4)))
    }

    /// Writes all fields of `c` (insert or update by user).
    pub(crate) fn store_contact(&self, c: &Contact, avatar_file: Option<i64>) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO contacts(account_id, user_id, profile_name, system_name, nickname,
                   avatar_file, trust, fingerprint, blocked, hidden, unregistered_at, identifiers,
                   meta, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(account_id, user_id) DO UPDATE SET
                   profile_name = excluded.profile_name, system_name = excluded.system_name,
                   nickname = excluded.nickname, avatar_file = excluded.avatar_file,
                   trust = excluded.trust, fingerprint = excluded.fingerprint,
                   blocked = excluded.blocked, hidden = excluded.hidden,
                   unregistered_at = excluded.unregistered_at, identifiers = excluded.identifiers,
                   meta = excluded.meta, updated_at = excluded.updated_at",
            )?
            .execute(params![
                self.acc.id.0,
                c.user.as_str(),
                c.profile_name,
                c.system_name,
                c.nickname,
                avatar_file,
                sql::enum_name(&c.trust)?,
                c.identity_fingerprint,
                c.blocked,
                c.hidden,
                c.unregistered_at,
                sql::to_json(&c.identifiers)?,
                c.meta.as_ref().map(sql::blob_to_sql),
                self.now,
            ])?;
        Ok(())
    }

    fn empty_contact(&self, user: UserId) -> Contact {
        Contact {
            id: ContactId(0),
            account: self.acc.id,
            user,
            profile_name: None,
            system_name: None,
            nickname: None,
            avatar: None,
            trust: TrustLevel::default(),
            identity_fingerprint: None,
            blocked: false,
            hidden: false,
            unregistered_at: None,
            identifiers: Vec::new(),
            meta: None,
        }
    }

    /// `ContactUpsert`: a field-wise patch, creating the contact when unknown.
    pub(crate) fn contact_upsert(&mut self, mut p: ContactPatch) -> Result<()> {
        let user = self.resolve_user(&p.user)?;
        p.user = user.clone();
        let (mut contact, avatar_file) = match self.load_contact(&user)? {
            Some(x) => x,
            None => (self.empty_contact(user.clone()), None),
        };
        let avatar_touched = !p.avatar.is_unchanged();
        p.apply(&mut contact);
        let avatar_file = if avatar_touched {
            self.store_avatar(avatar_file, contact.avatar.as_ref())?
        } else {
            avatar_file
        };
        self.store_contact(&contact, avatar_file)?;
        self.ch.contacts = true;
        // The direct conversation shows the contact's name and avatar.
        if let Some(id) = sql::conversation_id(self.conn, self.acc.id, &ConvKey::Direct(user))? {
            self.ch.conversations.insert(id);
        }
        Ok(())
    }

    // ---------- conversations ----------

    /// Loads the roaming part of a conversation into the model type (local fields default).
    fn load_conversation(&self, id: ConversationId, key: ConvKey) -> Result<(Conversation, bool)> {
        type Row = (
            bool,
            Option<String>,
            Option<i64>,
            Option<i64>,
            bool,
            Option<i64>,
            bool,
            bool,
            bool,
            Option<i64>,
            Option<String>,
            Option<Vec<u8>>,
        );
        let r: Row = self
            .conn
            .prepare_cached(
                "SELECT is_self, title, avatar_file, muted_until, archived, pinned_order, blocked,
                   request_pending, marked_unread, timer_secs, features, meta
                 FROM conversations WHERE id = ?1",
            )?
            .query_row([id.0], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                ))
            })?;
        let members: Vec<Member> = self
            .conn
            .prepare_cached(
                "SELECT user_id, role FROM members WHERE conversation_id = ?1 ORDER BY rowid",
            )?
            .query_map([id.0], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(u, role)| {
                Ok(Member {
                    user: sql::user(&u)?,
                    role: sql::from_enum_name(&role)?,
                })
            })
            .collect::<Result<_>>()?;
        let features_known = r.10.is_some();
        let features: ConversationFeatures = sql::from_json_opt(r.10)?.unwrap_or_default();
        let conv = Conversation {
            id,
            account: self.acc.id,
            kind: key.kind(),
            key,
            is_self: r.0,
            title: r.1,
            avatar: self.load_file(r.2)?,
            members,
            muted_until: r.3,
            archived: r.4,
            pinned_order: r.5.map(sql::u32_of),
            blocked: r.6,
            request_pending: r.7,
            marked_unread: r.8,
            timer: r
                .9
                .map(|s| Duration::from_secs(u64::try_from(s.max(0)).unwrap_or(0))),
            parent: None,
            features,
            local_muted_until: None,
            draft: None,
            unread: 0,
            mentions: 0,
            read_up_to: None,
            last_activity: 0,
            meta: sql::blob_from_sql(r.11),
        };
        Ok((conv, features_known))
    }

    /// `ConversationUpsert`: a field-wise patch, creating the conversation when unknown.
    pub(crate) fn conversation_upsert(&mut self, mut p: ConversationPatch) -> Result<()> {
        let key = self.canon_conv(&p.key)?;
        let id = self.ensure_conversation(&key)?;
        let (avatar_file, timer_version, hash_before): (Option<i64>, Option<i64>, Option<i64>) =
            self.conn
                .prepare_cached(
                    "SELECT avatar_file, timer_version, features_hash FROM conversations
                     WHERE id = ?1",
                )?
                .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let (mut conv, features_known) = self.load_conversation(id, key.clone())?;
        // What the action flags of this conversation's rows depend on (`crate::actions`).
        let admin = |conv: &Conversation, own: &[UserId]| {
            conv.members
                .iter()
                .any(|m| m.role == kourier_model::MemberRole::Admin && own.contains(&m.user))
        };
        let flags_before = (hash_before, conv.blocked, admin(&conv, &self.acc.own_ids));
        let members_before: std::collections::BTreeSet<UserId> =
            conv.members.iter().map(|m| m.user.clone()).collect();
        let was_untrusted = conv.blocked || conv.request_pending;
        // The network's version orders timer changes (§4.2 `ChangeConversation`): the timer of
        // a patch older than what we have is dropped, so any arrival order ends the same.
        let timer_version = match (p.timer_version.take().map(i64::from), timer_version) {
            (Some(new), Some(have)) if new < have => {
                p.timer = Patch::Unchanged;
                Some(have)
            }
            (Some(new), _) => Some(new),
            (None, have) => have,
        };

        if let Some(list) = &mut p.members {
            for m in list.upsert.iter_mut() {
                m.user = self.resolve_user(&m.user)?;
            }
            for u in list.remove.iter_mut() {
                *u = self.resolve_user(u)?;
            }
        }
        let parent = std::mem::take(&mut p.parent);
        let unread_hint = p.unread_hint.take();
        let features_set = p.features.is_some();
        let members_set = p.members.is_some();
        let avatar_touched = !p.avatar.is_unchanged();
        if key.direct_peer().is_some_and(|peer| self.is_own(peer)) {
            p.is_self = Some(true);
        }
        p.key = key;
        p.apply(&mut conv);

        let avatar_file = if avatar_touched {
            self.store_avatar(avatar_file, conv.avatar.as_ref())?
        } else {
            avatar_file
        };
        let (features, features_hash) = if features_set || features_known {
            (
                Some(sql::to_json(&conv.features)?),
                Some(i64::from_ne_bytes(
                    conv.features.stable_hash().to_ne_bytes(),
                )),
            )
        } else {
            (None, None)
        };
        let parent_id: Option<Option<i64>> = match parent {
            Patch::Unchanged => None,
            Patch::Clear => Some(None),
            Patch::Set(k) => {
                let k = self.canon_conv(&k)?;
                Some(Some(self.ensure_conversation(&k)?.0))
            }
        };
        self.conn
            .prepare_cached(
                "UPDATE conversations SET is_self = ?2, title = ?3, avatar_file = ?4,
                   muted_until = ?5, archived = ?6, pinned_order = ?7, blocked = ?8,
                   request_pending = ?9, marked_unread = ?10, timer_secs = ?11, features = ?12,
                   features_hash = ?13, meta = ?14,
                   parent_id = CASE WHEN ?15 THEN ?16 ELSE parent_id END, timer_version = ?17
                 WHERE id = ?1",
            )?
            .execute(params![
                id.0,
                conv.is_self,
                conv.title,
                avatar_file,
                conv.muted_until,
                conv.archived,
                conv.pinned_order,
                conv.blocked,
                conv.request_pending,
                conv.marked_unread,
                conv.timer
                    .map(|t| i64::try_from(t.as_secs()).unwrap_or(i64::MAX)),
                features,
                features_hash,
                conv.meta.as_ref().map(sql::blob_to_sql),
                parent_id.is_some(),
                parent_id.flatten(),
                timer_version,
            ])?;
        if members_set {
            self.conn
                .prepare_cached("DELETE FROM members WHERE conversation_id = ?1")?
                .execute([id.0])?;
            let mut ins = self.conn.prepare_cached(
                "INSERT OR REPLACE INTO members(conversation_id, user_id, role) VALUES (?1, ?2, ?3)",
            )?;
            for m in &conv.members {
                ins.execute((id.0, m.user.as_str(), sql::enum_name(&m.role)?))?;
            }
            drop(ins);
            // Someone joined the list: a receipt of theirs for a message we sent here waited
            // because they were not known as a recipient (§4.9), and counts now.
            if conv
                .members
                .iter()
                .any(|m| !members_before.contains(&m.user))
            {
                self.retry_receipts_in(id)?;
            }
        }
        if let Some(hint) = unread_hint {
            self.conn
                .prepare_cached("UPDATE conversations SET unread_hint = ?2 WHERE id = ?1")?
                .execute((id.0, i64::from(hint)))?;
            self.ch.recount_conv(id);
        }
        self.ch.conversations.insert(id);
        // New features, a block or a change of our role change what the user may do with the
        // messages that are there: their rows are read again.
        let flags_after = (features_hash, conv.blocked, admin(&conv, &self.acc.own_ids));
        if flags_after != flags_before {
            self.ch.timelines.insert(id);
        }
        if was_untrusted && !(conv.blocked || conv.request_pending) {
            // The request was accepted or the block lifted: the texts that arrived cut
            // off while nothing was fetched from this chat are completed now.
            self.fetch_long_texts(id)?;
        }
        Ok(())
    }

    /// The full ordered list of pinned conversations.
    pub(crate) fn pinned(&mut self, keys: &[ConvKey]) -> Result<()> {
        let mut pinned = Vec::with_capacity(keys.len());
        for k in keys {
            let k = self.canon_conv(k)?;
            pinned.push(self.ensure_conversation(&k)?);
        }
        let before: Vec<(i64, Option<i64>)> = self
            .conn
            .prepare_cached(
                "SELECT id, pinned_order FROM conversations
                 WHERE account_id = ?1 AND pinned_order IS NOT NULL",
            )?
            .query_map([self.acc.id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (id, _) in before {
            if !pinned.contains(&ConversationId(id)) {
                self.conn
                    .prepare_cached("UPDATE conversations SET pinned_order = NULL WHERE id = ?1")?
                    .execute([id])?;
                self.ch.conversations.insert(ConversationId(id));
            }
        }
        for (i, id) in pinned.iter().enumerate() {
            let n = self
                .conn
                .prepare_cached(
                    "UPDATE conversations SET pinned_order = ?2
                     WHERE id = ?1 AND pinned_order IS NOT ?2",
                )?
                .execute((id.0, i64::try_from(i).unwrap_or(i64::MAX)))?;
            if n > 0 {
                self.ch.conversations.insert(*id);
            }
        }
        Ok(())
    }

    /// The conversation is gone on the network: rows, messages and their files go.
    pub(crate) fn conversation_removed(&mut self, key: &ConvKey) -> Result<()> {
        let key = self.canon_conv(key)?;
        let Some(id) = sql::conversation_id(self.conn, self.acc.id, &key)? else {
            return Ok(());
        };
        let messages: Vec<i64> = self
            .conn
            .prepare_cached("SELECT id FROM messages WHERE conversation_id = ?1")?
            .query_map([id.0], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for m in messages {
            self.delete_attachments(kourier_model::MessageId(m))?;
        }
        self.conn
            .prepare_cached(
                "DELETE FROM pending_modifiers WHERE account_id = ?1 AND conv_key = ?2",
            )?
            .execute((self.acc.id.0, key.encode()))?;
        self.conn
            .prepare_cached("DELETE FROM conversations WHERE id = ?1")?
            .execute([id.0])?;
        self.ch.conversations.remove(&id);
        self.ch.messages.remove(&id);
        self.ch.removed.insert(id);
        Ok(())
    }
}

impl From<kourier_model::IdError> for CoreError {
    fn from(_: kourier_model::IdError) -> Self {
        CoreError::Json("malformed id".into())
    }
}

/// The primary own ids of the other accounts of `id`'s network that are not being removed,
/// each with whether that account is logged out, in the order of their ids. An account that
/// has no own id yet (a link in progress) has none to name.
pub(crate) fn other_identities(
    conn: &kourier_db::rusqlite::Connection,
    id: kourier_model::AccountId,
) -> Result<Vec<(UserId, bool)>> {
    let rows: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT state, own_ids FROM accounts
             WHERE id <> ?1 AND state <> 'Removing'
               AND network = (SELECT network FROM accounts WHERE id = ?1)
             ORDER BY id",
        )?
        .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (state, own) in rows {
        let own: Vec<UserId> = sql::from_json(&own)?;
        if let Some(primary) = own.into_iter().next() {
            out.push((primary, state == "LoggedOut"));
        }
    }
    Ok(out)
}

/// Whether another account of `id`'s network, not being removed, has `primary` as its primary
/// own id; `Some(logged_out)` of the first such account.
fn same_identity(
    conn: &kourier_db::rusqlite::Connection,
    id: kourier_model::AccountId,
    primary: Option<&UserId>,
) -> Result<Option<bool>> {
    let Some(primary) = primary else {
        return Ok(None);
    };
    Ok(other_identities(conn, id)?
        .into_iter()
        .find(|(user, _)| user == primary)
        .map(|(_, logged_out)| logged_out))
}
