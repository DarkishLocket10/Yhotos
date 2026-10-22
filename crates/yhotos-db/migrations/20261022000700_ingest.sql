-- Pipeline publication is atomic with its version marker and follow-up jobs.
CREATE TYPE photo_status AS ENUM ('pending','processing','ready','failed');
ALTER TABLE photos ADD COLUMN status photo_status NOT NULL DEFAULT 'pending',
 ADD COLUMN failure_reason text CHECK (char_length(failure_reason) <= 200),
 ADD COLUMN derivative_plan text;
CREATE INDEX jobs_dedupe ON jobs(kind) WHERE finished_at IS NULL;
