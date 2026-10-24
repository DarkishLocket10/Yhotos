CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX photos_title_trgm ON photos USING gin (title gin_trgm_ops);
ALTER TABLE faces ADD COLUMN model text NOT NULL DEFAULT 'legacy';
ALTER TABLE ocr_text ADD COLUMN model text NOT NULL DEFAULT 'legacy';
ALTER TABLE photo_tags ADD COLUMN model text;
ALTER TABLE faces ADD CONSTRAINT faces_bbox_normalized CHECK (
  bbox[1] BETWEEN 0 AND 1 AND bbox[2] BETWEEN 0 AND 1 AND
  bbox[3] BETWEEN 0 AND 1 AND bbox[4] BETWEEN 0 AND 1 AND bbox[1]<bbox[3] AND bbox[2]<bbox[4]);
ALTER TABLE ocr_text ADD CONSTRAINT ocr_text_cap CHECK (octet_length(text)<=4096);
REVOKE ALL ON faces,persons FROM yhotos_public;
