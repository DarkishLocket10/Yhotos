-- Record exact import targets so reruns can remove obsolete/ambiguous mappings
-- without deleting unrelated redirects or mappings manually retargeted later.
CREATE TABLE migration_redirects (
 pattern text PRIMARY KEY,
 target_kind target_kind NOT NULL,
 target_id uuid NOT NULL
);
GRANT ALL ON migration_redirects TO yhotos_admin;

-- Adopt mappings from imports made before ownership was recorded explicitly.
INSERT INTO migration_redirects(pattern,target_kind,target_id)
SELECT r.pattern,r.target_kind,r.target_id FROM legacy_redirects r
WHERE (r.target_kind='album' AND EXISTS (
 SELECT 1 FROM migration_albums a WHERE a.album_id=r.target_id
)) OR (r.target_kind='photo' AND EXISTS (
 SELECT 1 FROM migration_sources s WHERE s.photo_id=r.target_id
));
