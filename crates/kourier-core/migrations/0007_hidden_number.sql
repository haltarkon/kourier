-- Phase H (architecture §13).
--
-- `conversations.hidden`: the chat was deleted on another own device
-- (`BackendEvent::ConversationCleared { remove: true }`) and nothing a person sent is left in it.
-- The row stays with its title, members and features; the conversation list leaves it out until
-- a message arrives or the user writes there, which clears the flag.
--
-- `accounts.number`: the account's own number or address as the network shows it to people
-- (`BackendEvent::AccountReady.number`; Signal: the E.164 number, which changes with a number
-- change on the phone). Personal data: never logged.
ALTER TABLE conversations ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN number TEXT;
