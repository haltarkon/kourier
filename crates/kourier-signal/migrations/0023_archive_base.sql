-- Signal backend.db, A1 archived chats (docs/architecture.md §5.5, the receive path): what a
-- chat's phone-side flags were AS LAST TOLD TO THE CORE, so that the rule "a new message brings
-- an archived chat back" can move the diff base with the unarchive, and so that it can be
-- decided with a column read (never a decode of `storage_records` per message).
--
-- `peers.roaming_archived`: 1:1 chats and Note to Self (the own ACI's row). NULL = never told
-- (a profile from before this migration, or a chat no record stated yet): the receive path
-- fills it once, offline, from the peer's newest stored record. 0/1 otherwise. Groups keep
-- theirs in `groups.archived` (the Storage merge writes the flags there first).
-- `peers.roaming_muted_until`: the phone's mute of that chat in milliseconds written with
-- it (NULL = not muted; i64::MAX = for ever), the other half of Signal's "Keep muted chats
-- archived". `storage_records.record` is never rewritten: a later write of the flags back to
-- the Storage Service needs the bytes the phone sent.
-- `account.keep_muted_archived`: AccountRecord.keepMutedChatsArchived (Desktop's `?? false`).
-- NULL = never read: the receive path fills it once, offline, from the newest stored account
-- record (false when there is none), like `peers.roaming_archived`; a profile from before this
-- migration has its record stored for ever and the phone sends a new one only when an account
-- setting changes. 0/1 otherwise; every account record merged writes it.
ALTER TABLE peers ADD COLUMN roaming_archived INTEGER CHECK (roaming_archived IN (0, 1));
ALTER TABLE peers ADD COLUMN roaming_muted_until INTEGER;
ALTER TABLE account ADD COLUMN keep_muted_archived INTEGER CHECK (keep_muted_archived IN (0, 1));
