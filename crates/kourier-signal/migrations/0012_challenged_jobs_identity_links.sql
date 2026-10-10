-- Signal backend.db, phase I (docs/architecture.md §6.12, §6.13).

-- A job that waits because the server set a send challenge (428): one that met it, and the
-- other jobs that send to other accounts, which are held without a request of their own. An
-- accepted answer makes exactly these due again; a job that waits out a rate limit of its own
-- keeps its time.
ALTER TABLE jobs ADD COLUMN challenged INTEGER NOT NULL DEFAULT 0 CHECK (challenged IN (0, 1));
-- When a held job was due before the challenge held it (NULL for the job that met the
-- challenge itself): an accepted answer gives a held job this time back, so one that was
-- waiting out a rate limit or a backoff of its own is not sent early.
ALTER TABLE jobs ADD COLUMN held_from INTEGER;

-- Which ACI a PNI belongs to, as far as this device learned it (a Storage Service contact
-- record with both ids, a verified PNI signature, a sealed sender's certificate that states
-- for an ACI the number a contact record gives to a PNI). One funnel writes it
-- (`links::learn`), and the core is told `IdentityMerged{pni → aci}` when a pair is new or
-- changed, whichever source it came from. A group change that promotes an invited PNI is not
-- a source (`links` says why).
CREATE TABLE identity_links(
  pni TEXT PRIMARY KEY,                   -- service id string of the PNI ("PNI:<uuid>")
  aci TEXT NOT NULL,                      -- service id string of the ACI
  learned_at INTEGER NOT NULL             -- ms since the epoch
) STRICT, WITHOUT ROWID;

-- Who holds a phone number: stage 1 asks for every sealed envelope whose sender certificate
-- states one (`links::number_of_aci`), so the answer must not cost a scan of `peers`.
CREATE INDEX peers_e164 ON peers(e164) WHERE e164 IS NOT NULL;
