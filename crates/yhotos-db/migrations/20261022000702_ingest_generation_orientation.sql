-- Display dimensions describe oriented derivatives, independently of source EXIF.
ALTER TABLE photos
 ADD COLUMN original_orientation integer CHECK (original_orientation BETWEEN 1 AND 8),
 ADD COLUMN generation bigint NOT NULL DEFAULT 0 CHECK (generation >= 0);
UPDATE photos SET original_orientation=orientation;
-- Earlier ingest already stored display dimensions but retained source orientation.
UPDATE photos SET orientation=1 WHERE derivative_plan IS NOT NULL;

-- Existing envelopes refer to the initial generation; retain their hash fence.
UPDATE jobs SET payload=jsonb_set(payload,'{generation}','0'::jsonb)
 WHERE kind='ingest' AND jsonb_typeof(payload)='object' AND NOT payload ? 'generation';
