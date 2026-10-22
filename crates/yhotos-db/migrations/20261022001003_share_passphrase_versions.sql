-- Bind each grant to the passphrase that actually authorized it. A landing may
-- refresh that grant, but can never recreate a protected membership.
ALTER TABLE share_links ADD COLUMN passphrase_version bigint NOT NULL DEFAULT 0
 CHECK (passphrase_version>=0);
ALTER TABLE share_session_links ADD COLUMN passphrase_version bigint NOT NULL DEFAULT 0
 CHECK (passphrase_version>=0);

CREATE FUNCTION share_passphrase_changed() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 IF NEW.passphrase_hash IS DISTINCT FROM OLD.passphrase_hash THEN
  NEW.passphrase_version=OLD.passphrase_version+1;
 ELSE
  NEW.passphrase_version=OLD.passphrase_version;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER share_passphrase_version BEFORE UPDATE ON share_links
 FOR EACH ROW EXECUTE FUNCTION share_passphrase_changed();
REVOKE ALL ON FUNCTION share_passphrase_changed() FROM PUBLIC;

-- The public role cannot UPDATE links. This seam returns the current hash under
-- a link lock, retained by the caller while it verifies and inserts the grant.
CREATE FUNCTION share_unlock_link(lid uuid) RETURNS SETOF public.share_links
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 SELECT * FROM public.share_links WHERE id=lid AND revoked_at IS NULL AND allow_view
 AND (expires_at IS NULL OR expires_at>clock_timestamp()) FOR SHARE;
$$;
ALTER FUNCTION share_unlock_link(uuid) OWNER TO yhotos_admin;
REVOKE ALL ON FUNCTION share_unlock_link(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION share_unlock_link(uuid) TO yhotos_public,yhotos_admin;

DROP FUNCTION share_session_link(uuid,text);
CREATE FUNCTION share_session_link(lid uuid, expected_version bigint)
RETURNS SETOF public.share_links
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 SELECT * FROM public.share_unlock_link(lid) WHERE passphrase_version=expected_version;
$$;
ALTER FUNCTION share_session_link(uuid,bigint) OWNER TO yhotos_admin;
REVOKE ALL ON FUNCTION share_session_link(uuid,bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION share_session_link(uuid,bigint) TO yhotos_public,yhotos_admin;
