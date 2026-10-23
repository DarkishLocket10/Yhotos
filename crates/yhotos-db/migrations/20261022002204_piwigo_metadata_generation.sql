-- Source overrides apply only to the ingest generation the import intended.
ALTER TABLE migration_sources
 ADD COLUMN generation bigint NOT NULL DEFAULT 0 CHECK (generation >= 0),
 ADD COLUMN metadata_edited boolean NOT NULL DEFAULT false;
UPDATE migration_sources m SET generation=p.generation FROM photos p
 WHERE p.id=m.photo_id;

CREATE OR REPLACE FUNCTION preserve_piwigo_metadata() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE imported record;
BEGIN
 SELECT * INTO imported FROM public.migration_sources
 WHERE photo_id=NEW.id AND original_hash=NEW.original_hash
   AND generation=NEW.generation;
 IF FOUND THEN
  IF imported.metadata_edited THEN
   -- Complete administrative metadata replacements (including NULL removals)
   -- remain authoritative when missing derivatives are rebuilt in this generation.
   NEW.taken_at := OLD.taken_at;
   NEW.taken_at_local := OLD.taken_at_local;
   NEW.gps_lat := OLD.gps_lat;
   NEW.gps_lon := OLD.gps_lon;
  ELSE
   NEW.taken_at := coalesce(imported.taken_at, NEW.taken_at);
   NEW.taken_at_local := coalesce(imported.taken_at_local, NEW.taken_at_local);
   NEW.gps_lat := coalesce(imported.gps_lat, NEW.gps_lat);
   NEW.gps_lon := coalesce(imported.gps_lon, NEW.gps_lon);
  END IF;
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION preserve_piwigo_metadata() FROM PUBLIC;
