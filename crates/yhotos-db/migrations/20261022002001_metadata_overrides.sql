-- Keep explicit admin overrides across retries and replacements.
ALTER TABLE photos ADD COLUMN taken_at_override timestamptz;
ALTER TABLE photos ADD COLUMN gps_overridden boolean NOT NULL DEFAULT false;
