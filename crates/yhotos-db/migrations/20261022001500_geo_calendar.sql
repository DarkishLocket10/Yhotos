-- Native PostgreSQL points keep this offline feature independent of PostGIS.
CREATE TABLE gazetteer (
 geonameid bigint PRIMARY KEY CHECK (geonameid > 0),
 name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
 admin1 text NOT NULL CHECK (length(admin1) <= 80),
 country text NOT NULL CHECK (country ~ '^[A-Z]{2}$'),
 lat double precision NOT NULL CHECK (lat BETWEEN -90 AND 90),
 lon double precision NOT NULL CHECK (lon BETWEEN -180 AND 180),
 population bigint NOT NULL CHECK (population >= 0),
 point point GENERATED ALWAYS AS (point(lon,lat)) STORED
);
CREATE INDEX gazetteer_point ON gazetteer USING gist(point);
ALTER TABLE places ADD COLUMN geonameid bigint UNIQUE;
ALTER TABLE photos ADD COLUMN gps_point point GENERATED ALWAYS AS
 (CASE WHEN gps_lat IS NOT NULL AND gps_lon IS NOT NULL THEN point(gps_lon,gps_lat) END) STORED;
CREATE INDEX photos_gps_point ON photos USING gist(gps_point)
 WHERE deleted_at IS NULL AND gps_point IS NOT NULL;
CREATE INDEX photos_calendar ON photos ((coalesce(taken_at,posted_at)), id)
 WHERE deleted_at IS NULL;
GRANT SELECT ON gazetteer TO yhotos_worker;
GRANT ALL ON gazetteer TO yhotos_admin;
