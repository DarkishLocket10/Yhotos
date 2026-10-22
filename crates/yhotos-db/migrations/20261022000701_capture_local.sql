-- Preserve camera wall-clock time independently of its interpreted UTC instant.
ALTER TABLE photos ADD COLUMN taken_at_local timestamp without time zone;
