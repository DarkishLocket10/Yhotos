-- Distance ordering bounds title work before fusing with OCR matches.
CREATE INDEX photos_title_distance ON photos USING gist (title gist_trgm_ops);
DROP INDEX photos_title_trgm;
