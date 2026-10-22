-- Preserve validated creation metadata exactly for tus HEAD responses.
ALTER TABLE uploads ADD COLUMN metadata text NOT NULL DEFAULT ''
 CHECK (octet_length(metadata) <= 4096);
