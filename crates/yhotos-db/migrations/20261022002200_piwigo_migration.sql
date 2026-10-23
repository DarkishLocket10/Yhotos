-- Only the operator's administrative login can read or change import identities.
CREATE TABLE migration_albums (
 source_id bigint PRIMARY KEY CHECK (source_id > 0),
 album_id uuid NOT NULL UNIQUE REFERENCES albums ON DELETE CASCADE
);
ALTER TABLE photos ADD COLUMN author text;
CREATE TABLE migration_sources (
 image_id bigint NOT NULL CHECK (image_id > 0),
 category_id bigint NOT NULL REFERENCES migration_albums(source_id),
 original_hash text NOT NULL CHECK (original_hash ~ '^[0-9a-f]{64}$'),
 photo_id uuid NOT NULL UNIQUE REFERENCES photos ON DELETE CASCADE,
 taken_at timestamptz, taken_at_local timestamp, gps_lat double precision, gps_lon double precision,
 PRIMARY KEY (image_id, category_id)
);
CREATE INDEX migration_sources_hash ON migration_sources(original_hash, image_id);
GRANT ALL ON migration_albums, migration_sources TO yhotos_admin;

-- Piwigo's curated fields take priority over subsequently extracted EXIF.
-- Fence by hash so a later admin original replacement does not inherit them.
CREATE FUNCTION preserve_piwigo_metadata() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE imported record;
BEGIN
 SELECT * INTO imported FROM public.migration_sources
 WHERE photo_id=NEW.id AND original_hash=NEW.original_hash;
 IF FOUND THEN
  NEW.taken_at := coalesce(imported.taken_at, NEW.taken_at);
  NEW.taken_at_local := coalesce(imported.taken_at_local, NEW.taken_at_local);
  NEW.gps_lat := coalesce(imported.gps_lat, NEW.gps_lat);
  NEW.gps_lon := coalesce(imported.gps_lon, NEW.gps_lon);
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION preserve_piwigo_metadata() FROM PUBLIC;
CREATE TRIGGER preserve_piwigo_metadata BEFORE UPDATE OF taken_at,taken_at_local,gps_lat,gps_lon ON photos
 FOR EACH ROW EXECUTE FUNCTION preserve_piwigo_metadata();
