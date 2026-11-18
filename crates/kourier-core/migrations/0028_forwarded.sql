-- Forwards (forward line F1): the network's count of earlier forwards of a message that reached
-- this chat as a forward (`kourier_model::Forwarded::score`, WhatsApp's semantics: 0 for a first
-- forward). NULL = not a forward, or the network does not say. Written for a NEW row only (live
-- and history); a second delivery, an edit and a merge keep the stored value. No row is converted.
ALTER TABLE messages ADD COLUMN forwarded INTEGER;
