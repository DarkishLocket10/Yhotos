-- An explicit empty place is an override too; geocoding must preserve it.
ALTER TABLE photos ADD COLUMN place_overridden boolean NOT NULL DEFAULT false;
