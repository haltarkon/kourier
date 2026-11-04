-- People pages, step P2 (2026-10-22): the group's description. `description` is the network's
-- whole text of a group (plain text, line breaks kept); NULL = never said, or none. No index, no
-- table rebuilt, no row converted: the Signal backend states it with each group fetch and, once
-- at the next start, for every stored group (its features hash moves with `group_info`).
ALTER TABLE conversations ADD COLUMN description TEXT;
