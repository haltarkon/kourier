-- People pages, step P1 (2026-10-20): what a person's page shows that the contact row did not
-- carry. `about` and `about_emoji` are the profile's "about" text (before, only a backend blob
-- held them, which the core could not read); `number_shown` is the backend's verdict whether the
-- person's phone number may be shown (1) or not (0); NULL = the backend never said, which counts
-- as NOT shown. No index, no table rebuilt, no row converted (a profile fetch fills them).
ALTER TABLE contacts ADD COLUMN about TEXT;
ALTER TABLE contacts ADD COLUMN about_emoji TEXT;
ALTER TABLE contacts ADD COLUMN number_shown INTEGER CHECK (number_shown IN (0, 1));
