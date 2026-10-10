-- What a durable command asked for (JSON `DurableCommand`), for the commands whose content
-- tests check: the deletes for me made on this device (`ClearConversation`,
-- `DeleteAttachment`), which a real backend would turn into a sync message for the account's
-- other devices. NULL for receipts and read syncs. Tests read it with `kourier_fake::job_log`.
ALTER TABLE jobs ADD COLUMN payload TEXT;
