-- Phase G (architecture §13).
--
-- `conversations.timer_version`: the network's version counter of the disappearing-message
-- timer, as the backend states it in `ConversationPatch.timer_version` (Signal
-- `expireTimerVersion`). The core keeps the highest version it saw and ignores the timer of a
-- patch with a lower one, so timer changes that arrive out of order converge. NULL: the network
-- has no such counter, or no versioned change arrived yet.
--
-- `attachments.long_text`: the attachment is the whole text of a message whose body arrived
-- trimmed (`Attachment.long_text`). It is downloaded whatever the download settings say; once the
-- file is here the text replaces the body and the row goes.
ALTER TABLE conversations ADD COLUMN timer_version INTEGER;
ALTER TABLE attachments ADD COLUMN long_text INTEGER NOT NULL DEFAULT 0;
