-- 3D viewer (V2): the model mark of a downloaded STL or 3MF file, written by the derive step in
-- the transaction that records its still picture (`thumb_file_id`).
--
-- `model_x_mm`, `model_y_mm`, `model_z_mm`: the bounding box in millimetres; `model_tris`: the
-- triangle count; `model_tint`: 1 when the still is a shade picture the front-end tints (the
-- model has no colours of its own), 0 when it is drawn in the model's own colours. All NULL for
-- every other file and for a model the core could not read. No row is converted.
ALTER TABLE attachments ADD COLUMN model_x_mm REAL;
ALTER TABLE attachments ADD COLUMN model_y_mm REAL;
ALTER TABLE attachments ADD COLUMN model_z_mm REAL;
ALTER TABLE attachments ADD COLUMN model_tris INTEGER;
ALTER TABLE attachments ADD COLUMN model_tint INTEGER;

-- Models that were downloaded before this step were looked at by the derive step and set aside
-- (`Outcome::Nothing` sets `derived_at`), so they would stay plain files for ever. Hand them back
-- to the derive step: `pending_derives` finds every row with `derived_at IS NULL` at the next
-- start. The rule is `kourier_mesh::looks_like_model(file_name, mime)` (name ends in `.stl` or
-- `.3mf`, or the MIME essence is one of its five types), limited as `DeriveJob::is_model` is: a
-- file that is downloaded, has no still yet, is no long text and no picture (kind Image or Gif,
-- an `image/` type, a sticker). Change the two together.
UPDATE attachments SET derived_at = NULL
 WHERE file_id IS NOT NULL AND thumb_file_id IS NULL AND long_text = 0
   AND derived_at IS NOT NULL
   AND kind NOT IN ('Image', 'Gif')
   AND lower(coalesce(mime, '')) NOT LIKE 'image/%'
   AND message_id NOT IN (SELECT id FROM messages WHERE body_kind = 'Sticker')
   AND (lower(coalesce(file_name, '')) LIKE '%.stl'
        OR lower(coalesce(file_name, '')) LIKE '%.3mf'
        OR lower(trim(substr(coalesce(mime, ''), 1, instr(coalesce(mime, '') || ';', ';') - 1))) IN (
             'model/stl', 'model/3mf', 'application/sla', 'application/vnd.ms-pki.stl',
             'application/vnd.ms-package.3dmanufacturing-3dmodel+xml'));
