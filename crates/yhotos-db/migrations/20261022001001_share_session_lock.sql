-- Public cannot UPDATE share_links, which SELECT FOR SHARE otherwise requires.
-- This fixed-search-path seam only locks a live link at the verified passphrase
-- version. The caller retains the lock through insertion and commit, so an
-- admin passphrase change sees and revokes every newly issued session.
CREATE FUNCTION share_session_link(lid uuid, expected_hash text)
RETURNS SETOF public.share_links
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 SELECT * FROM public.share_links
 WHERE id=lid AND revoked_at IS NULL AND allow_view
 AND (expires_at IS NULL OR expires_at>clock_timestamp())
 AND passphrase_hash IS NOT DISTINCT FROM expected_hash
 FOR SHARE;
$$;
ALTER FUNCTION share_session_link(uuid,text) OWNER TO yhotos_admin;
REVOKE ALL ON FUNCTION share_session_link(uuid,text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION share_session_link(uuid,text) TO yhotos_public,yhotos_admin;
