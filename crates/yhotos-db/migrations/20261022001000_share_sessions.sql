-- A bearer authenticates exactly one account or one share link, never both.
ALTER TABLE sessions ALTER COLUMN user_id DROP NOT NULL;
ALTER TABLE sessions ADD COLUMN share_link_id uuid REFERENCES share_links ON DELETE CASCADE;
ALTER TABLE sessions ADD CONSTRAINT sessions_exactly_one_actor
 CHECK ((user_id IS NULL) <> (share_link_id IS NULL));
ALTER TABLE sessions ADD CONSTRAINT sessions_share_public
 CHECK (share_link_id IS NULL OR (NOT admin AND mfa_at IS NULL AND reauth_at IS NULL));
CREATE INDEX sessions_share_link ON sessions(share_link_id) WHERE share_link_id IS NOT NULL;

CREATE FUNCTION share_cache_invalidate() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 PERFORM pg_notify('session_invalidate', 'link:' || OLD.id::text);
 -- A new passphrase invalidates every previously unlocked bearer.
 IF TG_OP = 'UPDATE' AND NEW.passphrase_hash IS DISTINCT FROM OLD.passphrase_hash THEN
  DELETE FROM public.sessions WHERE share_link_id=OLD.id;
 END IF;
 RETURN NULL;
END $$;
-- Deleting sessions requires the admin role; public has no share-link writes.
CREATE TRIGGER share_cache_changed AFTER UPDATE OR DELETE ON share_links
 FOR EACH ROW EXECUTE FUNCTION share_cache_invalidate();
REVOKE ALL ON FUNCTION share_cache_invalidate() FROM PUBLIC;

-- One cold landing read includes display metadata and complete access chains.
-- Invoker permissions apply; no elevated table access is granted by this helper.
CREATE FUNCTION share_target_photos(link_kind target_kind, link_target uuid) RETURNS jsonb
LANGUAGE sql STABLE SET search_path=pg_catalog,public,pg_temp AS $$
 WITH RECURSIVE subtree AS (
  SELECT a.id, ARRAY[a.id] AS visited FROM public.albums a
   WHERE link_kind='album' AND a.id=link_target AND a.deleted_at IS NULL
  UNION ALL
  SELECT a.id, s.visited || a.id FROM subtree s JOIN public.albums a ON a.parent_id=s.id
   WHERE a.deleted_at IS NULL AND NOT a.id=ANY(s.visited)
 ), selected AS (
  SELECT p.* FROM public.photos p WHERE p.deleted_at IS NULL
   AND ((link_kind='photo' AND p.id=link_target) OR (link_kind='album' AND p.album_id IN (SELECT id FROM subtree)))
 ), chain AS (
  SELECT p.id AS photo_id,a.id,a.parent_id,a.visibility,a.deleted_at,0 AS depth,ARRAY[a.id] AS visited
   FROM selected p JOIN public.albums a ON a.id=p.album_id
  UNION ALL
  SELECT c.photo_id,a.id,a.parent_id,a.visibility,a.deleted_at,c.depth+1,c.visited || a.id
   FROM chain c JOIN public.albums a ON a.id=c.parent_id WHERE NOT a.id=ANY(c.visited)
 )
 SELECT coalesce(jsonb_agg(jsonb_build_object(
  'id',p.id,'title',p.title,'visibility',p.visibility,'original_hash',p.original_hash,
  'width',coalesce(d.width,p.width,1),'height',coalesce(d.height,p.height,1),
  'image',CASE WHEN d.hash IS NOT NULL THEN '/i/' || d.hash || '/' || d.variant || '.' || d.format || CASE WHEN d.hdr THEN '.hdr' ELSE '' END END,
  'users',ARRAY(SELECT principal_id FROM public.photo_grants WHERE photo_id=p.id AND principal_kind='user'),
  'groups',ARRAY(SELECT principal_id FROM public.photo_grants WHERE photo_id=p.id AND principal_kind='group'),
  'albums',(SELECT jsonb_agg(jsonb_build_object('id',c.id,'visibility',c.visibility,
   'users',ARRAY(SELECT principal_id FROM public.album_grants WHERE album_id=c.id AND principal_kind='user'),
   'groups',ARRAY(SELECT principal_id FROM public.album_grants WHERE album_id=c.id AND principal_kind='group')) ORDER BY c.depth)
   FROM chain c WHERE c.photo_id=p.id)
 ) ORDER BY p.sort_order,p.id),'[]'::jsonb)
 FROM selected p LEFT JOIN LATERAL (
  SELECT hash,variant,format,hdr,width,height FROM public.derivatives WHERE photo_id=p.id
   ORDER BY (variant='small') DESC,(format='jpeg') DESC,hdr,variant,format LIMIT 1
 ) d ON true
 WHERE EXISTS(SELECT FROM chain c WHERE c.photo_id=p.id AND c.parent_id IS NULL)
  AND NOT EXISTS(SELECT FROM chain c WHERE c.photo_id=p.id AND c.deleted_at IS NOT NULL);
$$;
REVOKE ALL ON FUNCTION share_target_photos(target_kind,uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION share_target_photos(target_kind,uuid) TO yhotos_public,yhotos_admin;
